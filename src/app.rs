//! AppState, event orchestration and the frame-loop skeleton (Notes/02 §3/§4;
//! Step 4 is the single place that maintains mpsc ordering).
//!
//! Design notes (Step 4 Prototype validation, `examples/proto_step4.rs`):
//! - single reducer: `handle(event) -> Vec<Cmd>`; UI/transport never hold a
//!   mutable model reference directly;
//! - page requests are **single-flight** (`in_flight`) + **generation** to
//!   guard against stale responses (out-of-order arrivals are dropped);
//! - **no HTTP page requests while disconnected/reconnecting** — only record
//!   `want_backfill`, and send it after the refollow snapshot completes
//!   (AC-001-12 reconnect-pagination reconciliation);
//! - any successful server response sets `Ready`; a disconnect sets
//!   `Reconnecting` (visible in the status bar, events are never swallowed
//!   silently);
//! - quit command order for a running session: cancel → terminal restore →
//!   exit (AC-001-08);
//! - permission errors (PERMISSION_DENIED) are not auto-retried (Notes/03 §8).

use std::collections::{HashMap, HashSet};

use crate::api::types::ChunkRow;
use crate::api::types::{
    ApprovalEvent, ApprovalOutcome, AttachmentId, ControlItem, ListItemRaw, PromptContentPart,
    PromptMode, PromptRequest, SearchHit, SessionHistoryRecord, SessionId, SessionLogOffset,
    SessionRequestId, SessionSeq, SessionWireEvent,
};
use crate::api::{ClientError, ErrorClass};
use crate::model::{
    block_plain_text, block_yank_target, selection_text, ApplyEffect, AttachmentRef, DraftRegistry,
    DraftState, ImageViewState, Incoming, InputHistory, SearchIndex, SearchKindFilter,
    SessionStore, VisualMode, VisualSelection, WorkspaceStore, YankBackend, YankState,
};

/// 已编码的 Kitty 帧（ratatui-image `Protocol` 对象；`Box<dyn Protocol>` 无
/// Debug，手工实现避免 AppState 丢失 Debug 派生）。
pub struct KittyFrame(pub Box<dyn ratatui_image::protocol::Protocol>);

impl std::fmt::Debug for KittyFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KittyFrame").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Picker,
    /// REQ-002 composer (V0.2 扩展：steer 标注 + 历史)。
    Insert,
    /// `/` 结构化搜索 overlay（REQ-003）。
    Search,
    /// `v`/`V` 视觉选择（REQ-003）。
    Visual,
    /// 审批到达强制进入的模态（REQ-003，`y/n/q/Esc` 决策后回先前模式）。
    Approval,
    /// ImageView（REQ-004 V0.2：仅 Kitty 渲染态出现，`q` 回 NORMAL）。
    ImageView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Sidebar,
    Center,
    Details,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Ready,
    Reconnecting,
    /// Startup probe/auth failure: show the "please start dsh web / check
    /// 127.0.0.1:3080" guidance (AC-001-02).
    StartupFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewport {
    /// Offset of the viewport top in the window block list (block index).
    pub offset: usize,
    /// Whether to stick to the live tail (streaming follow; frozen when the
    /// user scrolls up).
    pub follow_tail: bool,
    /// Visible height (lines).
    pub height: usize,
    /// 焦点块锚点（REQ-004 引入，供 REQ-003「光标在链接上」复用）：当前
    /// 焦点块 seq（最小实现 = 视口内首个可见图片块，随滚动/重建刷新）。
    pub focused_seq: Option<SessionSeq>,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            offset: 0,
            follow_tail: true,
            height: 24,
            focused_seq: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PickerState {
    pub open: bool,
    pub query: String,
    pub selection: usize,
}

/// Modal composer state (REQ-002 §5): visible only in INSERT.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComposerState {
    pub visible: bool,
    /// Send target session; always set while visible — empty means not
    /// sendable (AC-002-12).
    pub active_session: Option<SessionId>,
    /// Running session → steer 模式（REQ-003 AC-003-06，状态条 STEER）。
    pub steer: bool,
}

/// SEARCH overlay state（REQ-003 §5 `SearchState`；全内存）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchState {
    pub open: bool,
    /// 原样输入（含 `/c` 前缀）。
    pub query: String,
    /// 窗口内即时命中（按窗口顺序）。
    pub window_matches: Vec<usize>,
    /// 当前匹配下标（`n`/`N` 巡览，0-based）。
    pub cursor: usize,
    /// 全历史 `session/search` 命中（会话级）。
    pub history_hits: Vec<SearchHit>,
    pub history_selection: usize,
    pub history_loading: bool,
    /// 防抖 generation：stale 结果/触发直接丢弃（模式 15 in-flight 去重）。
    pub history_generation: u64,
    pub history_error: Option<String>,
    /// hasMore=true 的用户可见提示（细化关键词，无翻页 RPC）。
    pub history_hint: Option<String>,
    /// Enter 后进入结果巡览：n/N/y/j/k 为命令；未锁定时它们是输入字符
    /// （AC-003-05 与「输入实时过滤」的模态内两段式）。
    pub results_locked: bool,
}

impl SearchState {
    /// 前缀过滤 + 有效查询词（`/c x` → (Code, "x")）。
    pub fn terms(&self) -> (SearchKindFilter, String) {
        let (filter, term) = SearchKindFilter::from_prefix(&self.query);
        (filter, term.to_string())
    }
}

/// APPROVAL 模态状态（REQ-003 §5 `ApprovalState`；仅内存）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApprovalState {
    pub visible: bool,
    pub event: Option<ApprovalEvent>,
    pub last_outcome: Option<ApprovalOutcome>,
    /// outcome 回复在途：同一弹窗只回复一次（幂等）。
    pub reply_inflight: bool,
    /// 进入审批前的模式（关闭后恢复，不丢运行状态）。
    pub prev_mode: Mode,
    /// 不可编程审批降级：状态条 `等待审批` + 指引官方 web（AC-003-18）。
    pub waiting_hint: bool,
    /// 一次性提示（toast）。
    pub toast: Option<String>,
}

/// turnOutline 大纲列表（`O`；D-19 独立键，与 `o` 打开不冲突）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutlineState {
    pub open: bool,
    pub selection: usize,
}

/// Local stop transition (REQ-002 §5): shows「停止中」while requested; ends
/// only when the official projection flips running=false (ADR-008, never
/// self-computed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StopState {
    pub requested_session: Option<SessionId>,
}

impl StopState {
    /// Whether a stop transition is in flight for the given session.
    pub fn is_requested_for(&self, sid: &SessionId) -> bool {
        self.requested_session.as_ref() == Some(sid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageGuard {
    pub generation: u64,
    pub in_flight: bool,
}

/// `loadThrough(seq)` 分页上限（REQ-003 §6：防无界分页；200 条/页口径）。
pub const LOAD_THROUGH_MAX_PAGES: usize = 20;

/// 官方 projections 的「待审批」候选键（wire 字段 `[未验证]`，容忍布尔/
/// 非空数组/状态字符串三种形状，AC-003-18 降级检测）。
const APPROVAL_PENDING_KEYS: &[&str] = &[
    "awaitingApproval",
    "waitingApproval",
    "pendingApproval",
    "approvalPending",
    "needsApproval",
];

/// 官方投影是否存在待审批信号（仅布尔 true/非空数组/状态字符串；缺失或
/// false 均视为无待审批，不臆造）。
pub fn projections_await_approval(projections: &serde_json::Value) -> bool {
    for key in APPROVAL_PENDING_KEYS {
        let Some(v) = projections.get(key) else {
            continue;
        };
        match v {
            serde_json::Value::Bool(b) => {
                if *b {
                    return true;
                }
            }
            serde_json::Value::Array(a) => {
                if !a.is_empty() {
                    return true;
                }
            }
            serde_json::Value::String(s)
                if matches!(s.as_str(), "pending" | "awaiting" | "waiting") =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Events entering AppState (from api tasks / the input layer / the frame
/// loop).
#[derive(Debug)]
pub enum AppEvent {
    /// Startup: after auth succeeds, load the session list first.
    Startup,
    SessionListPage {
        items: Vec<ListItemRaw>,
        next_cursor: Option<String>,
    },
    SessionListError(ClientError),
    /// workspace/follow raw frame (the api layer has already tolerated unknown
    /// shapes).
    WorkspaceFrame(serde_json::Value),
    FollowSnapshot {
        session_id: SessionId,
        cursor: Option<SessionLogOffset>,
        records: Vec<SessionHistoryRecord>,
        has_more: bool,
        projections: Option<serde_json::Value>,
    },
    FollowEvent {
        session_id: SessionId,
        event: SessionWireEvent,
    },
    FollowChunks {
        session_id: SessionId,
        row: ChunkRow,
    },
    FollowError {
        session_id: SessionId,
        error: ClientError,
    },
    /// `session/prompt` accepted receipt (ends this command's state only;
    /// durable follow is the sole reconciliation source, REQ-002 Step 3).
    PromptAccepted {
        session_id: SessionId,
        request_id: SessionRequestId,
    },
    /// `session/prompt` failure (marks pending error + status hint, never
    /// auto-resent).
    PromptFailed {
        session_id: SessionId,
        request_id: SessionRequestId,
        error: ClientError,
    },
    /// `session/cancel` accepted (stop transition held until the official
    /// projection flips).
    CancelAccepted {
        session_id: SessionId,
    },
    /// `session/cancel` failure (visible hint, no crash, retryable).
    CancelFailed {
        session_id: SessionId,
        error: ClientError,
    },
    PageResult {
        session_id: SessionId,
        generation: u64,
        records: Vec<SessionHistoryRecord>,
        has_more: Option<bool>,
    },
    PageError {
        session_id: SessionId,
        generation: u64,
        error: ClientError,
    },
    Disconnected(String),
    Reconnected,
    /// Startup probe failed (AC-001-02 guidance).
    StartupProbeFailed(String),
    Resize {
        width: u16,
        height: u16,
    },
    /// 搜索防抖计时到点（generation 校验后才会真正发 `session/search`）。
    SearchHistoryDebounced {
        query: String,
        generation: u64,
    },
    /// `session/search` 结果（stale 直接丢弃，AC-003-14）。
    SearchResult {
        generation: u64,
        items: Vec<SearchHit>,
        has_more: bool,
    },
    SearchError {
        generation: u64,
        error: ClientError,
    },
    /// 转发的 `approval/request` waterfall 事件（AC-003-07）。
    ApprovalRequest {
        event: ApprovalEvent,
    },
    /// outcome 回复成功/失败（失败 fail-closed，AC-003-17）。
    ApprovalReplied {
        outcome: ApprovalOutcome,
    },
    ApprovalReplyFailed {
        outcome: ApprovalOutcome,
        error: ClientError,
    },
    /// `session/control` 流帧（baseline 的 projections.running 读入官方口径）。
    ControlItem {
        session_id: SessionId,
        item: ControlItem,
    },
    /// `loadThrough(seq)` 的一页（按 REQ-001 seq 幂等合并进窗口）。
    LoadThroughPage {
        session_id: SessionId,
        records: Vec<SessionHistoryRecord>,
        has_more: Option<bool>,
    },
    /// 剪贴板写入结果（AC-003-08 降级链反馈）。
    CopyDone {
        backend: YankBackend,
        ok: bool,
    },
    // ---------- REQ-004 V0.2 图片事件 ----------
    /// 拉取+解码+kitty 编码成功（spawn_blocking 回写）。
    AttachmentReady {
        session_id: SessionId,
        attachment_id: AttachmentId,
        block_seq: SessionSeq,
        /// 拉取元数据（占位回填）。
        meta: AttachmentRef,
        /// 已编码 Kitty 帧（非 Kitty 路径为 None）。
        frame: Option<KittyFrame>,
        /// 缓存条目（临时文件已落盘）。
        entry: crate::model::ImageCacheEntry,
        /// true = 缓存命中重解码（不再 complete 入缓存）。
        cached: bool,
        /// 本次打开是否为「非 Kitty 直达系统查看器」。
        for_viewer: bool,
    },
    /// 拉取/解码失败。
    AttachmentFailed {
        session_id: SessionId,
        attachment_id: AttachmentId,
        block_seq: SessionSeq,
        /// Remote `error.code` / `network` / `decode/unsupported` /
        /// `decode/corrupt` / `encode/failed`（§6 分类依据）。
        code: String,
        /// 可读提示。
        message: String,
        /// 网络断连 → 既有指数退避重连；权限/格式类不自动重试。
        retryable: bool,
        for_viewer: bool,
    },
}

/// Orchestration commands emitted by the reducer (executed by the run loop).
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    LoadSessionList {
        cursor: Option<String>,
    },
    OpenWorkspaceFollow,
    OpenFollow {
        session_id: SessionId,
        max_messages: usize,
    },
    RequestPage {
        session_id: SessionId,
        generation: u64,
        through_seq: SessionSeq,
        before_seq: Option<SessionSeq>,
        max_messages: usize,
    },
    Reconnect {
        delay_ms: u64,
    },
    CancelSession(SessionId),
    /// REQ-002 single send command (produced by submit_input; take-once
    /// guards against double Enter).
    SendPrompt {
        session_id: SessionId,
        request: PromptRequest,
    },
    RestoreTerminal,
    Exit,
    /// 全历史 `session/search`（防抖后触发；generation 校验）。
    SearchSessions {
        query: String,
        generation: u64,
    },
    /// 300ms 防抖计时命令（execute_one 异步等待后回投 Debounced 事件）。
    DebounceSearch {
        query: String,
        generation: u64,
    },
    /// 打开 `session/control` 流。
    OpenControl {
        session_id: SessionId,
    },
    /// 审批 outcome 回复（unary）。
    ReplyApproval {
        event: ApprovalEvent,
        outcome: ApprovalOutcome,
    },
    /// 系统打开链接（仅用户显式触发，REQ-003 §3）。
    OpenExternal {
        target: String,
    },
    /// 跳轮专用分页：拉到覆盖目标 seq。
    LoadThrough {
        seq: SessionSeq,
    },
    /// 写系统剪贴板（arboard → OSC52 降级链在 execute_one 执行）。
    CopyToClipboard {
        text: String,
    },
    // ---------- REQ-004 V0.2 ----------
    /// `session/attachment` 拉取 + 解码 + kitty 编码（单飞已在 reducer 判定）。
    FetchAttachment {
        session_id: SessionId,
        attachment_id: AttachmentId,
        block_seq: SessionSeq,
        /// 非 Kitty：成功后直达系统查看器（不进入 ImageView）。
        for_viewer: bool,
    },
    /// 从缓存临时文件重新解码渲染（缓存命中路径）。
    RenderCachedImage {
        session_id: SessionId,
        attachment_id: AttachmentId,
        block_seq: SessionSeq,
        temp_file: std::path::PathBuf,
        media_type: crate::api::types::MediaType,
    },
    /// 系统查看器打开原图（`open`/`xdg-open` 子进程不阻塞）。
    OpenSystemViewer {
        path: std::path::PathBuf,
    },
    /// 复制图片路径/附件名到剪贴板（arboard，不可用降级提示）。
    CopyImageText {
        text: String,
    },
}

#[derive(Debug)]
pub struct AppState {
    pub mode: Mode,
    pub focus: Focus,
    pub conn: ConnState,
    pub active_session: Option<SessionId>,
    pub sessions: SessionStore,
    pub workspaces: WorkspaceStore,
    pub viewport: Viewport,
    pub picker: PickerState,
    pub composer: ComposerState,
    /// Active composer buffer (bound to a session; the registry keeps drafts
    /// across session switches, D-20).
    pub draft: Option<DraftState>,
    /// Cross-session draft registry (memory only, LRU 20).
    pub drafts: DraftRegistry,
    /// Global input history (↑/↓, ≤50, memory only).
    pub history: InputHistory,
    /// 搜索 overlay + 窗口索引（随窗口重建）。
    pub search: SearchState,
    pub search_index: SearchIndex,
    /// 视觉选择/剪贴板。
    pub yank: YankState,
    /// 审批模态。
    pub approval: ApprovalState,
    /// turnOutline 大纲列表（`O`）。
    pub outline: OutlineState,
    /// 焦点块游标（窗口块下标；搜索跳转/视觉选择/上下文 yank 的锚）。
    pub cursor_block: usize,
    /// Local stop transition (from `s` until the official projection flips).
    pub stop: StopState,
    pub help_open: bool,
    pub quit_requested: bool,
    pub exited: bool,
    /// Most recent user-facing error (shown in the status bar/guidance area,
    /// no spam).
    pub last_error: Option<String>,
    /// 一次性成功反馈（如 `copied`，状态条显示，04 §4.4）。
    pub notice: Option<String>,
    /// Startup guidance (AC-001-02).
    pub startup_guidance: Option<String>,
    /// Terminal size (render breakpoint input).
    pub width: u16,
    pub height: u16,
    /// Window message cap (config).
    pub window_cap: usize,
    page_guard: PageGuard,
    want_backfill: bool,
    /// `loadThrough(seq)` 在途目标：每页合并后 reducer 判断是否已覆盖，
    /// 未覆盖且仍有更多历史则继续发下一页（AC-003-09 按 seq 落位）。
    load_through_target: Option<SessionSeq>,
    load_through_pages: usize,
    running_sessions: HashSet<SessionId>,
    /// Workspaces collapsed via `h` (FR-001-03); `l` expands all.
    pub collapsed_workspaces: HashSet<crate::api::types::WorkspaceId>,
    list_cursor: Option<String>,
    list_loaded: bool,
    // ---------- REQ-004 V0.2 图片 ----------
    /// 终端 Kitty 能力（启动检测一次，06 §6）：true → ImageView 渲染路径，
    /// false → 占位 `o`/`Enter` 直达系统查看器（AC-004-07）。
    pub kitty_capable: bool,
    /// IMAGEVIEW 模式状态（来源块锚点 + 拉取/解码/渲染阶段）。
    pub image_view: ImageViewState,
    /// 已渲染的 Kitty 帧（Rendered 阶段持有）。
    pub image_frame: Option<KittyFrame>,
    /// 拉取成功的附件元数据（attachment_id → AttachmentRef；占位回填标注）。
    pub image_meta: HashMap<AttachmentId, AttachmentRef>,
    /// 拉取/解码失败（attachment_id → 可读提示；错误占位）。
    pub image_errors: HashMap<AttachmentId, String>,
    /// 拉取/解码在途（attachment_id；幂等判定与加载指示）。
    pub image_loading: HashSet<AttachmentId>,
    /// kitty image id 分配源（每帧唯一）。
    kitty_frame_id: u16,
    /// 图片缓存（LRU + 预算 + 单飞，Arc 供 spawn_blocking 共享）。
    pub image_cache: std::sync::Arc<crate::cache::image_cache::ImageCache>,
    /// 未入缓存的临时文件（拉取即弃/系统查看器持有），进程退出清理。
    transient_files: Vec<std::path::PathBuf>,
    /// 当前 ImageView 展示附件的临时文件路径（未入缓存时系统查看器/复制用）。
    view_temp_path: Option<std::path::PathBuf>,
    /// Non-Kitty viewer request target; stale completions must not launch a viewer.
    pub pending_viewer: Option<(SessionId, SessionSeq, AttachmentId)>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            mode: Mode::Normal,
            focus: Focus::Sidebar,
            conn: ConnState::Connecting,
            active_session: None,
            sessions: SessionStore::new(3),
            workspaces: WorkspaceStore::new(),
            viewport: Viewport::default(),
            picker: PickerState::default(),
            composer: ComposerState::default(),
            draft: None,
            drafts: DraftRegistry::new(20),
            history: InputHistory::new(50),
            search: SearchState::default(),
            search_index: SearchIndex::new(),
            yank: YankState::default(),
            approval: ApprovalState::default(),
            outline: OutlineState::default(),
            cursor_block: 0,
            stop: StopState::default(),
            help_open: false,
            quit_requested: false,
            exited: false,
            last_error: None,
            notice: None,
            startup_guidance: None,
            width: 80,
            height: 24,
            window_cap: 200,
            page_guard: PageGuard::default(),
            want_backfill: false,
            load_through_target: None,
            load_through_pages: 0,
            running_sessions: HashSet::new(),
            collapsed_workspaces: HashSet::new(),
            list_cursor: None,
            list_loaded: false,
            kitty_capable: false,
            image_view: ImageViewState::default(),
            image_frame: None,
            image_meta: HashMap::new(),
            image_errors: HashMap::new(),
            image_loading: HashSet::new(),
            kitty_frame_id: 0,
            image_cache: std::sync::Arc::new(crate::cache::image_cache::ImageCache::new(
                crate::config::DEFAULT_CACHE_BYTES,
            )),
            transient_files: Vec::new(),
            view_temp_path: None,
            pending_viewer: None,
        }
    }
}

impl AppState {
    pub fn new(window_cap: usize) -> Self {
        Self {
            window_cap,
            ..Self::default()
        }
    }

    // ---------- queries (UI read-only) ----------

    pub fn is_reconnecting(&self) -> bool {
        self.conn == ConnState::Reconnecting
    }

    pub fn active_window(&self) -> Option<&crate::model::TranscriptWindow> {
        self.active_session
            .as_ref()
            .and_then(|id| self.sessions.get(&id.0))
    }

    pub fn active_running(&self) -> bool {
        self.active_session
            .as_ref()
            .map(|id| self.running_sessions.contains(id))
            .unwrap_or(false)
    }

    /// `loadThrough(seq)` downstream contract (REQ-003): jump the viewport to a
    /// turn seq that is inside the loaded window. Returns false when the seq
    /// is not loaded (caller decides whether to page through to it).
    pub fn scroll_to_seq(&mut self, seq: SessionSeq) -> bool {
        let Some(window) = self.active_window() else {
            return false;
        };
        let Some(index) = window.offset_of(seq) else {
            return false;
        };
        self.viewport.follow_tail = false;
        self.viewport.offset = index;
        true
    }

    /// AC-001-02 guidance text (shown when the probe fails; includes
    /// retry/quit hints).
    pub fn guidance_text(&self) -> String {
        match &self.startup_guidance {
            Some(g) => format!(
                "请启动 dsh web / 检查 127.0.0.1:3080（dsh web --host 127.0.0.1 --port 3080）后重试。\n{g}\n[r] 重试  [q] 退出"
            ),
            None => String::new(),
        }
    }

    // ---------- reducer ----------

    pub fn handle(&mut self, ev: AppEvent) -> Vec<Cmd> {
        match ev {
            AppEvent::Startup => {
                self.list_loaded = false;
                vec![
                    Cmd::LoadSessionList { cursor: None },
                    Cmd::OpenWorkspaceFollow,
                ]
            }
            AppEvent::SessionListPage { items, next_cursor } => {
                self.conn = ConnState::Ready;
                for raw in items {
                    if let Some(meta) = crate::api::types::meta_from_raw(raw) {
                        self.workspaces.upsert_session(meta);
                    }
                }
                if next_cursor.is_some() && !self.list_loaded {
                    // Keep paging until the list is fully loaded (local
                    // incremental merge, Notes/06 §1).
                    self.list_cursor = next_cursor;
                    vec![Cmd::LoadSessionList {
                        cursor: self.list_cursor.clone(),
                    }]
                } else {
                    self.list_cursor = None;
                    self.list_loaded = true;
                    vec![]
                }
            }
            AppEvent::SessionListError(e) => {
                self.list_cursor = None;
                self.record_error(e, "会话列表加载失败");
                vec![]
            }
            AppEvent::WorkspaceFrame(frame) => {
                // Field-level frame parsing lives in api/workspace (protocol
                // knowledge centralized in the api layer).
                if let Some(items) = crate::api::workspace::extract_workspaces(&frame) {
                    for item in items {
                        let wid = crate::api::types::WorkspaceId(item.id);
                        self.workspaces.upsert_workspace(wid.clone(), item.title);
                        for session in item.session_ids {
                            self.workspaces
                                .attach_session_to_workspace(&wid, &SessionId(session));
                        }
                    }
                }
                vec![]
            }
            AppEvent::FollowSnapshot {
                session_id,
                cursor,
                records,
                has_more,
                projections,
            } => {
                // Any successful server response proves the connection is
                // ready.
                self.conn = ConnState::Ready;
                // NOTE: active_session is only ever set by open_session. A
                // stale snapshot from a previously opened session must not
                // steal the selection after the user switched away
                // (per-session windows are keyed by session id, so the data
                // still lands correctly).
                // `running` comes from the official projections (never
                // self-computed).
                let running = projections
                    .as_ref()
                    .and_then(|p| p.get("running"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // AC-003-18 检测：官方投影出现待审批信号且无弹窗事件 →
                // 收缩为状态条 `等待审批`（不转发 approval/request 的
                // 目标版本路径）。
                if self.approval.event.is_none() {
                    if let Some(p) = projections.as_ref() {
                        self.approval.waiting_hint = projections_await_approval(p);
                    }
                }
                if running {
                    self.running_sessions.insert(session_id.clone());
                } else {
                    self.running_sessions.remove(&session_id);
                    // Official projection flips running=false → stop
                    // transition ends (ADR-008, projections only).
                    if self.stop.is_requested_for(&session_id) {
                        self.stop.requested_session = None;
                    }
                }
                let eff = {
                    let w = self.sessions.touch(&session_id.0, self.window_cap);
                    w.apply(Incoming::Snapshot {
                        cursor,
                        records,
                        has_more,
                        projections,
                    })
                };
                self.adjust_viewport(&eff);
                self.window_changed();
                // Reconnect reconciliation: send the browsed gap after the
                // refollow completes (AC-001-12).
                if self.want_backfill {
                    self.want_backfill = false;
                    vec![self.page_cmd()]
                } else {
                    vec![]
                }
            }
            AppEvent::FollowEvent { session_id, event } => {
                let Some(w) = self.sessions.get_mut(&session_id.0) else {
                    tracing::warn!(session = %session_id, "事件到达但窗口不存在，丢弃");
                    return vec![];
                };
                let eff = w.apply(Incoming::FollowEvent(event));
                self.adjust_viewport(&eff);
                self.window_changed();
                vec![]
            }
            AppEvent::FollowChunks { session_id, row } => {
                let Some(w) = self.sessions.get_mut(&session_id.0) else {
                    return vec![];
                };
                let eff = w.apply(Incoming::Chunks(row));
                self.adjust_viewport(&eff);
                self.window_changed();
                vec![]
            }
            AppEvent::FollowError { session_id, error } => {
                if error.class() == ErrorClass::PermissionDenied {
                    // Permission errors are not auto-retried (Notes/03 §8)
                    // and do not disconnect.
                    self.last_error = Some(format!("权限不足（{}）：{}", session_id, error));
                    vec![]
                } else {
                    // Other stream errors are treated as a disconnect →
                    // uniform reconnect orchestration.
                    self.handle_disconnected(format!("follow 流错误: {error}"))
                }
            }
            AppEvent::PageResult {
                session_id,
                generation,
                records,
                has_more,
            } => {
                // generation check: a stale response is dropped directly
                // (single-flight + out-of-order guard).
                if !self.page_guard.in_flight || generation != self.page_guard.generation {
                    tracing::debug!(generation, "stale page 响应丢弃");
                    return vec![];
                }
                self.page_guard.in_flight = false;
                let eff = self
                    .sessions
                    .get_mut(&session_id.0)
                    .map(|w| w.apply(Incoming::Page { records, has_more }));
                if let Some(eff) = eff {
                    self.adjust_viewport(&eff);
                    self.window_changed();
                }
                vec![]
            }
            AppEvent::PageError {
                session_id,
                generation,
                error,
            } => {
                if !self.page_guard.in_flight || generation != self.page_guard.generation {
                    return vec![];
                }
                self.page_guard.in_flight = false;
                self.record_error(error, "历史加载失败");
                let _ = session_id;
                vec![]
            }
            AppEvent::PromptAccepted {
                session_id,
                request_id,
            } => {
                // The success receipt only ends this command's state; the
                // pending echo is retired solely by durable follow events
                // (official source of truth, AC-002-06).
                tracing::debug!(%session_id, %request_id, "session/prompt accepted");
                vec![]
            }
            AppEvent::PromptFailed {
                session_id,
                request_id,
                error,
            } => {
                let code = error.code();
                let message = error.to_string();
                if let Some(w) = self.sessions.get_mut(&session_id.0) {
                    w.fail_echo(&request_id, &code, &message);
                }
                // AC-003-16: steer 不被接受（轮次已结束/agent 非运行）→ 状态条
                // 提示 + 草稿保留（回显文本放回注册表），应用不崩溃。
                if code == "session/steer-unavailable" {
                    let echo_text = self
                        .sessions
                        .get(&session_id.0)
                        .and_then(|w| w.echo_text(&request_id))
                        .map(str::to_string);
                    if let Some(text) = echo_text {
                        self.drafts.set(DraftState {
                            text,
                            cursor: 0,
                            bound_session: session_id.clone(),
                        });
                    }
                    self.last_error = Some(
                        "steer 不可用（轮次已结束或 agent 未运行），草稿已保留，可改为排队发送"
                            .to_string(),
                    );
                    tracing::warn!(%session_id, "session/steer-unavailable，草稿保留");
                    return vec![];
                }
                // AC-002-08: the status bar always shows a hint for a failed
                // send — including network-class errors (a dead follow stream
                // additionally shows reconnecting).
                match error.class() {
                    ErrorClass::Retryable => {
                        tracing::warn!(error = %message, "发送失败（网络，不自动重发）");
                        self.last_error = Some(format!("发送失败（网络）: {message}"));
                    }
                    _ => {
                        tracing::error!(error = %message, "发送失败（不自动重发）");
                        self.last_error = Some(format!("发送失败: {message}"));
                    }
                }
                vec![] // never auto-resent (AC-002-08)
            }
            AppEvent::CancelAccepted { session_id } => {
                // Accepted keeps the local stopping transition until the
                // official projection flips (AC-002-05/10; results for
                // non-requested sessions are silently ignored).
                if self.stop.is_requested_for(&session_id) {
                    tracing::debug!(%session_id, "session/cancel accepted（等待官方投影翻转）");
                }
                vec![]
            }
            AppEvent::CancelFailed { session_id, error } => {
                // Visible hint, no crash, transition released for retry
                // (AC-002-09); stale failures for other sessions are ignored.
                if self.stop.is_requested_for(&session_id) {
                    self.stop.requested_session = None;
                    let message = error.to_string();
                    match error.class() {
                        ErrorClass::Retryable => {
                            tracing::warn!(error = %message, "停止失败（网络）");
                            self.last_error = Some(format!("停止失败（网络）: {message}"));
                        }
                        _ => {
                            tracing::error!(error = %message, "停止失败");
                            self.last_error = Some(format!("停止失败: {message}"));
                        }
                    }
                } else {
                    tracing::debug!(%session_id, "stale cancel 失败忽略");
                }
                vec![]
            }
            AppEvent::Disconnected(reason) => self.handle_disconnected(reason),
            AppEvent::Reconnected => {
                self.conn = ConnState::Ready;
                // After recovery trigger refollow only once (no repeated
                // repair); REQ-003 重开 control 流（运行态/审批降级状态读取）。
                match self.active_session.clone() {
                    Some(sid) => vec![
                        Cmd::OpenFollow {
                            session_id: sid.clone(),
                            max_messages: self.window_cap,
                        },
                        Cmd::OpenControl { session_id: sid },
                    ],
                    None => vec![Cmd::LoadSessionList { cursor: None }],
                }
            }
            AppEvent::StartupProbeFailed(reason) => {
                self.conn = ConnState::StartupFailed;
                self.startup_guidance = Some(reason);
                self.last_error = None;
                vec![]
            }
            AppEvent::Resize { width, height } => {
                self.width = width;
                self.height = height;
                self.viewport.height = height.saturating_sub(2).max(1) as usize;
                self.refresh_focus();
                vec![]
            }
            AppEvent::SearchHistoryDebounced { query, generation } => {
                // 防抖到点 + generation 校验（模式 15：未收敛的异步信号丢弃
                // stale 触发；AC-003-14 只保留最新）。
                if generation != self.search.history_generation || query.trim().is_empty() {
                    tracing::debug!(generation, "stale 搜索防抖触发丢弃");
                    return vec![];
                }
                self.search.history_loading = true;
                vec![Cmd::SearchSessions { query, generation }]
            }
            AppEvent::SearchResult {
                generation,
                items,
                has_more,
            } => {
                if generation != self.search.history_generation {
                    tracing::debug!(generation, "stale search 结果丢弃");
                    return vec![];
                }
                self.search.history_loading = false;
                self.search.history_error = None;
                self.search.history_hint = None;
                self.search.history_hits = items;
                self.search.history_selection = 0;
                if has_more {
                    // 用户可见提示（§4：达上限 20 → 提示细化关键词，无翻页）。
                    self.search.history_hint =
                        Some("命中已达上限（20），请细化关键词（hasMore）".to_string());
                    tracing::debug!("session/search hasMore=true（提示细化关键词，无翻页）");
                }
                vec![]
            }
            AppEvent::SearchError { generation, error } => {
                if generation != self.search.history_generation {
                    return vec![];
                }
                self.search.history_loading = false;
                let msg = error.to_string();
                // 权限错误不自动重试（Notes/03 §8）；窗口内即时搜索不受影响
                // （AC-003-13）。
                match error.class() {
                    ErrorClass::PermissionDenied => {
                        tracing::warn!(error = %msg, "session/search 权限拒绝");
                        self.search.history_error = Some(format!("搜索权限不足: {msg}"));
                    }
                    _ => {
                        tracing::warn!(error = %msg, "session/search 失败");
                        self.search.history_error = Some(format!("全历史搜索失败: {msg}"));
                    }
                }
                vec![]
            }
            AppEvent::ApprovalRequest { event } => {
                // 同一事件重复投递（重连/重放）只保留一次（模式 15 去重）。
                if self
                    .approval
                    .event
                    .as_ref()
                    .is_some_and(|cur| cur.event_id == event.event_id)
                {
                    tracing::debug!(event_id = %event.event_id, "重复审批事件忽略");
                    return vec![];
                }
                if self.mode != Mode::Approval {
                    self.approval.prev_mode = self.mode;
                }
                self.mode = Mode::Approval;
                self.approval.event = Some(event);
                self.approval.visible = true;
                self.approval.reply_inflight = false;
                self.approval.waiting_hint = false;
                vec![]
            }
            AppEvent::ApprovalReplied { outcome } => {
                self.approval.reply_inflight = false;
                self.approval.last_outcome = Some(outcome);
                self.approval.toast = Some(format!("审批已回复: {}", outcome.as_str()));
                self.approval.event = None;
                self.approval.visible = false;
                self.mode = self.approval.prev_mode;
                vec![]
            }
            AppEvent::ApprovalReplyFailed { outcome, error } => {
                // fail closed：不授权；弹窗收缩为状态条 `等待审批` + 指引官方
                // web（AC-003-17/18），运行状态不丢。
                self.approval.reply_inflight = false;
                self.approval.event = None;
                self.approval.visible = false;
                self.approval.waiting_hint = true;
                self.mode = self.approval.prev_mode;
                self.last_error = Some(format!(
                    "审批回复失败（{}，不授权）: {error}；请在官方 web 完成审批",
                    outcome.as_str()
                ));
                vec![]
            }
            AppEvent::ControlItem { session_id, item } => {
                // 只消费官方 projection 的 running 事实（ADR-008）；其余
                // queue/jobs 帧本版本不解释。
                if let ControlItem::Baseline { projections, .. } = item {
                    let running = projections
                        .get("running")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    // AC-003-18 检测：官方投影出现待审批信号且无弹窗事件 →
                    // 收缩为状态条 `等待审批`（不转发 approval/request 的
                    // 目标版本路径）。
                    if self.approval.event.is_none() {
                        self.approval.waiting_hint = projections_await_approval(&projections);
                    }
                    if running {
                        self.running_sessions.insert(session_id.clone());
                    } else {
                        self.running_sessions.remove(&session_id);
                        if self.stop.is_requested_for(&session_id) {
                            self.stop.requested_session = None;
                        }
                    }
                }
                vec![]
            }
            AppEvent::LoadThroughPage {
                session_id,
                records,
                has_more,
            } => {
                // 与 follow/page 增量同漏斗：REQ-001 seq/requestId 去重
                // （AC-003-15）。
                let eff = self
                    .sessions
                    .get_mut(&session_id.0)
                    .map(|w| w.apply(Incoming::Page { records, has_more }));
                if let Some(eff) = eff {
                    self.adjust_viewport(&eff);
                    self.window_changed();
                }
                // AC-003-09 落位：目标 seq 已覆盖 → 落位并结束；未覆盖且
                // 仍有更多历史 → 发下一页（每页一次命令，渲染不被阻塞）；
                // 页数/历史耗尽 → 提示并结束。
                let Some(target) = self.load_through_target else {
                    return vec![];
                };
                if self.scroll_to_seq(target) {
                    if let Some(idx) = self.active_window().and_then(|w| w.offset_of(target)) {
                        self.cursor_block = idx;
                    }
                    self.load_through_target = None;
                    self.load_through_pages = 0;
                    return vec![];
                }
                if has_more == Some(true) {
                    self.load_through_pages += 1;
                    if self.load_through_pages >= LOAD_THROUGH_MAX_PAGES {
                        tracing::warn!(seq = %target, "loadThrough 达到分页上限仍未覆盖目标轮");
                        self.last_error = Some("目标轮不在可达历史范围内".to_string());
                        self.load_through_target = None;
                        self.load_through_pages = 0;
                        return vec![];
                    }
                    return vec![Cmd::LoadThrough { seq: target }];
                }
                self.last_error = Some("目标轮不在已加载历史范围内".to_string());
                self.load_through_target = None;
                self.load_through_pages = 0;
                vec![]
            }
            AppEvent::CopyDone { backend, ok } => {
                self.yank.backend = backend;
                if ok {
                    self.yank.toast = Some("copied".to_string());
                } else {
                    self.yank.backend = YankBackend::Unavailable;
                    self.yank.toast = None;
                    self.last_error =
                        Some("剪贴板不可用（系统剪贴板与 OSC52 均失败），内容未复制".to_string());
                }
                vec![]
            }
            AppEvent::AttachmentReady {
                session_id,
                attachment_id,
                block_seq,
                meta,
                frame,
                entry,
                cached,
                for_viewer,
            } => self.on_attachment_ready(
                session_id,
                attachment_id,
                block_seq,
                meta,
                frame,
                entry,
                cached,
                for_viewer,
            ),
            AppEvent::AttachmentFailed {
                session_id,
                attachment_id,
                block_seq,
                code,
                message,
                retryable,
                for_viewer,
            } => self.on_attachment_failed(
                session_id,
                attachment_id,
                block_seq,
                code,
                message,
                retryable,
                for_viewer,
            ),
        }
    }

    /// 窗口内容变化后的统一维护：重建搜索索引、刷新打开的窗口搜索、钳制
    /// 焦点块游标。
    fn window_changed(&mut self) {
        let blocks = self
            .active_window()
            .map(|w| w.block_snapshot())
            .unwrap_or_default();
        self.search_index.rebuild(&blocks);
        if self.search.open {
            self.recompute_window_matches();
        }
        let len = blocks.len();
        if self.cursor_block >= len {
            self.cursor_block = len.saturating_sub(1);
        }
    }

    /// 用当前查询词重算窗口内即时命中（离线，无网络）。
    fn recompute_window_matches(&mut self) {
        let (filter, term) = self.search.terms();
        let matches = self.search_index.query(&term, filter);
        self.search.window_matches = matches.iter().map(|m| m.item_index).collect();
        if self.search.window_matches.is_empty()
            || self.search.cursor >= self.search.window_matches.len()
        {
            self.search.cursor = 0;
        }
    }

    fn handle_disconnected(&mut self, reason: String) -> Vec<Cmd> {
        self.conn = ConnState::Reconnecting;
        // Void the in-flight page (generation is kept, guarding against stale
        // resurrection).
        self.page_guard.in_flight = false;
        // 断线中止 loadThrough（恢复后可由跳轮重新发起，目标不被旧状态污染）。
        self.load_through_target = None;
        self.load_through_pages = 0;
        tracing::warn!(%reason, "连接断开 → reconnecting");
        vec![Cmd::Reconnect { delay_ms: 500 }]
    }

    fn page_cmd(&mut self) -> Cmd {
        self.page_guard.generation += 1;
        self.page_guard.in_flight = true;
        let session_id = self.active_session.clone().expect("page_cmd 需要活动会话");
        // throughSeq = follow snapshot cursor; beforeSeq = oldest seq in the
        // window.
        let through_seq = self
            .sessions
            .get(&session_id.0)
            .and_then(|w| w.cursor())
            .map(|c| SessionSeq(c.0))
            .unwrap_or(SessionSeq(0));
        let before_seq = self.sessions.get(&session_id.0).and_then(|w| w.head_seq());
        Cmd::RequestPage {
            session_id,
            generation: self.page_guard.generation,
            through_seq,
            before_seq,
            max_messages: 50,
        }
    }

    /// ApplyEffect → viewport shift (the key to jitter-free scrolling).
    fn adjust_viewport(&mut self, eff: &ApplyEffect) {
        match eff {
            ApplyEffect::Rebuilt => {
                // Full-window rebuild: stick to the tail.
                self.viewport.follow_tail = true;
                self.scroll_to_bottom();
            }
            ApplyEffect::TailAppended { anchor_stable, .. } => {
                if self.viewport.follow_tail {
                    self.scroll_to_bottom();
                } else if !*anchor_stable {
                    // Head was evicted: shift the viewport forward in sync,
                    // keeping the relative browsing position.
                    self.viewport.offset = self.viewport.offset.saturating_sub(1);
                }
                // anchor_stable while browsing: do not move (no jitter).
            }
            ApplyEffect::HeadPrepend { anchor_shift, .. } => {
                // Prepend: shift the viewport by anchor_shift, the browsing
                // position does not jump.
                self.viewport.offset += anchor_shift;
            }
            ApplyEffect::Noop => {}
        }
        self.refresh_focus();
    }

    fn scroll_to_bottom(&mut self) {
        // Visible lines = durable blocks + local echo lines (pending renders
        // at the tail).
        let len = self
            .active_window()
            .map(|w| w.len() + w.pending().count())
            .unwrap_or(0);
        let height = self.viewport.height.max(1);
        self.viewport.offset = len.saturating_sub(height);
    }

    fn record_error(&mut self, e: ClientError, prefix: &str) {
        // Permission/business errors surface to the user; network errors only
        // go to the log + status bar (no spam).
        let msg = format!("{prefix}: {e}");
        match e.class() {
            ErrorClass::Retryable => tracing::warn!(error = %msg, "可重试错误"),
            _ => {
                tracing::error!(error = %msg, "错误");
                self.last_error = Some(msg);
            }
        }
    }

    // ---------- composer lifecycle (REQ-002 Step 2/3 entries) ----------

    /// `i` entry (AC-002-12 + REQ-003 D-20): without an active session stay
    /// NORMAL with a hint; with one, restore the session draft from the
    /// registry (cross-session retention, memory only) and mark steer when the
    /// session is running (official projection, AC-003-06).
    fn open_composer(&mut self) -> Vec<Cmd> {
        if self.mode == Mode::Insert && self.composer.visible {
            return vec![];
        }
        let Some(sid) = self.active_session.clone() else {
            self.composer.visible = false;
            self.composer.active_session = None;
            self.last_error = Some("无打开的会话（f/o 打开会话后再输入）".to_string());
            return vec![];
        };
        let restored = self
            .drafts
            .get(&sid)
            .map(|d| (d.text.clone(), d.cursor))
            .unwrap_or_default();
        self.draft = Some(DraftState {
            text: restored.0,
            cursor: restored.1,
            bound_session: sid.clone(),
        });
        self.history.reset_nav();
        self.mode = Mode::Insert;
        self.composer.visible = true;
        self.composer.active_session = Some(sid);
        // 运行态经官方 projections（active_running），不臆造（ADR-008）。
        self.composer.steer = self.active_running();
        vec![]
    }

    /// Esc: close the composer but keep the session-bound draft (AC-002-04;
    /// REQ-003 D-20 注册表持久到进程退出). `self.draft` 保留为会话内活草稿
    /// （V0.1 行为不变），注册表同步副本供跨会话恢复。
    fn close_composer_keep_draft(&mut self) {
        self.mode = Mode::Normal;
        self.composer.visible = false;
        self.composer.active_session = None;
        self.composer.steer = false;
        if let Some(d) = self.draft.as_ref() {
            self.drafts.set(d.clone());
        }
    }

    /// Insert text at the cursor (single line + Ctrl/Alt+Enter newline chars).
    fn composer_input(&mut self, text: &str) -> Vec<Cmd> {
        let Some(d) = self.draft.as_mut() else {
            return vec![];
        };
        if d.cursor > d.text.chars().count() {
            d.cursor = d.text.chars().count();
        }
        let mut out = String::with_capacity(d.text.len() + text.len());
        for (i, ch) in d.text.chars().enumerate() {
            if i == d.cursor {
                out.push_str(text);
            }
            out.push(ch);
        }
        if d.cursor >= d.text.chars().count() {
            out.push_str(text);
        }
        d.text = out;
        d.cursor += text.chars().count();
        vec![]
    }

    fn composer_backspace(&mut self) -> Vec<Cmd> {
        let Some(d) = self.draft.as_mut() else {
            return vec![];
        };
        if d.cursor == 0 {
            return vec![];
        }
        let idx = d.cursor - 1;
        let mut out = String::with_capacity(d.text.len());
        for (i, ch) in d.text.chars().enumerate() {
            if i != idx {
                out.push(ch);
            }
        }
        d.text = out;
        d.cursor = idx;
        vec![]
    }

    /// The single send entry: non-empty draft is taken, a requestId is
    /// minted, the optimistic echo lands immediately, INSERT exits and
    /// exactly one `Cmd::SendPrompt` is produced; empty input (whitespace
    /// included) stays in INSERT and sends nothing (AC-002-02/03/13).
    pub fn submit_input(&mut self, mode: PromptMode) -> Vec<Cmd> {
        if self.mode != Mode::Insert || !self.composer.visible {
            return vec![];
        }
        let Some(sid) = self.composer.active_session.clone() else {
            return vec![];
        };
        let Some(d) = self.draft.as_mut() else {
            return vec![];
        };
        if d.text.trim().is_empty() {
            // Empty input: send nothing, stay in INSERT (AC-002-03).
            return vec![];
        }
        // take-once + single command queue = minimal in-flight guard
        // (pattern 15 lesson: unconverged async signals need in-flight
        // dedup; AC-002-13 blocks double-Enter).
        let text = std::mem::take(&mut d.text);
        d.cursor = 0;
        self.mode = Mode::Normal;
        self.composer.visible = false;
        self.composer.steer = false;
        self.composer.active_session = None;
        // 发送后清空该会话草稿（AC-003-11）+ 记入输入历史（AC-003-10）。
        self.drafts.clear(&sid);
        self.history.push(&text);
        self.history.reset_nav();
        let request_id = SessionRequestId(crate::api::types::mint_request_id());
        // Optimistic echo: visible within one frame, occupies no seq
        // (AC-002-02).
        self.sessions
            .touch(&sid.0, self.window_cap)
            .echo(request_id.clone(), &text);
        let request = PromptRequest {
            request_id: request_id.clone(),
            session_id: sid.clone(),
            mode,
            content: vec![PromptContentPart::Text { text }],
            client_time_zone: None,
        };
        vec![Cmd::SendPrompt {
            session_id: sid,
            request,
        }]
    }

    /// `s` entry (AC-002-05/10): first press sends exactly one cancel; the
    /// local stopping transition holds until the official projection flips;
    /// a repeated `s` for the same session is idempotent; not running → noop.
    pub fn request_stop(&mut self) -> Vec<Cmd> {
        let Some(sid) = self.active_session.clone() else {
            return vec![];
        };
        if self.stop.is_requested_for(&sid) {
            return vec![]; // Same session already stopping: idempotent.
        }
        if !self.running_sessions.contains(&sid) {
            return vec![]; // Official projection says not running: noop.
        }
        self.stop.requested_session = Some(sid.clone());
        vec![Cmd::CancelSession(sid)]
    }

    // ---------- REQ-004 V0.2 图片状态机 ----------

    /// 焦点块锚点刷新：最小实现 = 视口内首个可见图片块 seq（随滚动/重建/
    /// resize 刷新；REQ-003「光标在链接上」复用同一锚点）。
    fn refresh_focus(&mut self) {
        self.viewport.focused_seq = self.focused_image_block().map(|b| b.seq);
    }

    /// 视口内首个可见图片块（供 `o`/`Enter` 打开与锚点维护）。
    pub fn focused_image_block(&self) -> Option<crate::model::ImageBlockRef> {
        let window = self.active_window()?;
        if let Some(seq) = self.viewport.focused_seq {
            if let Some(block) = window
                .blocks()
                .find(|block| block.seq() == seq)
                .and_then(crate::model::image_block_of)
            {
                return Some(block);
            }
        }
        let height = self.viewport.height.max(1);
        let start = self.viewport.offset;
        let end = (start + height).min(window.len());
        window
            .blocks()
            .skip(start)
            .take(end.saturating_sub(start))
            .find_map(crate::model::image_block_of)
    }

    fn viewer_target_matches(
        &self,
        session_id: &SessionId,
        block_seq: SessionSeq,
        attachment_id: &AttachmentId,
    ) -> bool {
        self.pending_viewer
            .as_ref()
            .is_some_and(|(target_session, target_seq, target_id)| {
                target_session == session_id
                    && *target_seq == block_seq
                    && target_id == attachment_id
            })
    }

    fn active_image_view_target(
        &self,
        session_id: &SessionId,
        block_seq: SessionSeq,
        attachment_id: &AttachmentId,
    ) -> bool {
        self.active_session.as_ref() == Some(session_id)
            && self.mode == Mode::ImageView
            && self.image_view.block_seq == Some(block_seq)
            && self.image_view.attachment_id.as_ref() == Some(attachment_id)
    }

    /// 当前 ImageView 展示附件的临时文件路径（缓存托管 → 缓存条目；
    /// 未入缓存 → view_temp_path）。
    fn view_image_path(&self) -> Option<std::path::PathBuf> {
        let att_id = self.image_view.attachment_id.as_ref()?;
        if let Some(entry) = self.image_cache.get(att_id) {
            return Some(entry.temp_file);
        }
        self.view_temp_path.clone()
    }

    /// 打开焦点图片块（两级键位 D-14）：
    /// - 已开任何 ImageView（V0.2 单图）→ 聚焦已有视图（幂等 no-op）；
    /// - 同 attachment_id 在途 → no-op（幂等，不重复 session/attachment）；
    /// - Kitty → ImageView（缓存命中重解码，未命中单飞拉取）；
    /// - 非 Kitty → 拉取落盘后直达系统查看器（AC-004-07）。
    fn open_focused_image(&mut self) -> Vec<Cmd> {
        let Some(block) = self.focused_image_block() else {
            return vec![];
        };
        if self.mode == Mode::ImageView {
            // AC-004-08：不叠加第二个 ImageView。
            return vec![];
        }
        let Some(att_id) = block.attachment_id.clone() else {
            // 防御：占位缺 attachment_id（不应发生）→ 提示不崩溃。
            self.last_error = Some("图片块缺少 attachment_id，无法打开".into());
            return vec![];
        };
        if self.image_loading.contains(&att_id) {
            // AC-004-08：在途幂等 no-op（聚焦已有加载）。
            return vec![];
        }
        let Some(session_id) = self.active_session.clone() else {
            return vec![];
        };
        match self.image_cache.acquire(&att_id) {
            crate::cache::image_cache::Acquire::Cached(entry) => {
                if !self.kitty_capable {
                    // 非 Kitty 缓存命中：直达系统查看器。无网络往返、无后续
                    // ready 事件——不登记 pending_viewer（避免残留状态）。
                    return vec![Cmd::OpenSystemViewer {
                        path: entry.temp_file,
                    }];
                }
                self.image_cache.pin(&att_id);
                self.image_view.open_view(
                    block.seq,
                    att_id.clone(),
                    block.name.clone(),
                    block.dims.clone(),
                );
                self.mode = Mode::ImageView;
                vec![Cmd::RenderCachedImage {
                    session_id,
                    attachment_id: att_id,
                    block_seq: block.seq,
                    temp_file: entry.temp_file,
                    media_type: entry.media_type,
                }]
            }
            crate::cache::image_cache::Acquire::InFlight => vec![],
            crate::cache::image_cache::Acquire::Started => {
                self.image_loading.insert(att_id.clone());
                if !self.kitty_capable {
                    // AC-004-07：非 Kitty 拉取后直达系统查看器，不进入
                    // ImageView。
                    self.pending_viewer = Some((session_id.clone(), block.seq, att_id.clone()));
                    return vec![Cmd::FetchAttachment {
                        session_id,
                        attachment_id: att_id,
                        block_seq: block.seq,
                        for_viewer: true,
                    }];
                }
                self.image_cache.pin(&att_id);
                self.image_view.open_view(
                    block.seq,
                    att_id.clone(),
                    block.name.clone(),
                    block.dims.clone(),
                );
                self.mode = Mode::ImageView;
                vec![Cmd::FetchAttachment {
                    session_id,
                    attachment_id: att_id,
                    block_seq: block.seq,
                    for_viewer: false,
                }]
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_attachment_ready(
        &mut self,
        session_id: SessionId,
        attachment_id: AttachmentId,
        block_seq: SessionSeq,
        meta: AttachmentRef,
        frame: Option<KittyFrame>,
        entry: crate::model::ImageCacheEntry,
        cached: bool,
        for_viewer: bool,
    ) -> Vec<Cmd> {
        let image_view_target = self.active_session.as_ref() == Some(&session_id)
            && self.mode == Mode::ImageView
            && self.image_view.block_seq == Some(block_seq)
            && self.image_view.attachment_id.as_ref() == Some(&attachment_id);
        let viewer_target = self.viewer_target_matches(&session_id, block_seq, &attachment_id);
        self.image_loading.remove(&attachment_id);
        if !cached && !image_view_target && !viewer_target {
            let _ = std::fs::remove_file(&entry.temp_file);
            self.image_cache.abort(&attachment_id);
            self.image_cache.unpin(&attachment_id);
            return vec![];
        }
        if !cached {
            self.image_meta.insert(attachment_id.clone(), meta);
            match self.image_cache.complete(&attachment_id, entry.clone()) {
                crate::cache::image_cache::InsertOutcome::Cached => {}
                crate::cache::image_cache::InsertOutcome::NotCached => {
                    if !image_view_target && !viewer_target {
                        let _ = std::fs::remove_file(&entry.temp_file);
                    } else {
                        self.transient_files.push(entry.temp_file.clone());
                        self.view_temp_path = Some(entry.temp_file.clone());
                    }
                }
            }
        }
        if !image_view_target && !viewer_target {
            self.image_cache.abort(&attachment_id);
            self.image_cache.unpin(&attachment_id);
            return vec![];
        }
        self.image_errors.remove(&attachment_id);
        if cached {
            // The cache entry already owns the file; do not re-account it.
            self.view_temp_path = None;
        }
        if for_viewer || !self.kitty_capable {
            // 非 Kitty：直达系统查看器（AC-004-07），模式保持 NORMAL。
            self.pending_viewer = None;
            return vec![Cmd::OpenSystemViewer {
                path: entry.temp_file,
            }];
        }
        // 防串图（AC-004-09）：视图已关/已换块 → 丢弃帧与状态。
        if self.mode != Mode::ImageView || self.image_view.block_seq != Some(block_seq) {
            return vec![];
        }
        match frame {
            Some(f) => {
                self.image_frame = Some(f);
                self.image_view.mark_rendered();
            }
            None => {
                self.image_view
                    .mark_failed("encode/failed".into(), "kitty 帧缺失".into());
            }
        }
        vec![]
    }

    #[allow(clippy::too_many_arguments)]
    fn on_attachment_failed(
        &mut self,
        _session_id: SessionId,
        attachment_id: AttachmentId,
        block_seq: SessionSeq,
        code: String,
        message: String,
        retryable: bool,
        for_viewer: bool,
    ) -> Vec<Cmd> {
        let current_target = self.active_image_view_target(&_session_id, block_seq, &attachment_id)
            || self.viewer_target_matches(&_session_id, block_seq, &attachment_id);
        self.image_loading.remove(&attachment_id);
        self.image_cache.abort(&attachment_id);
        self.image_cache.unpin(&attachment_id);
        if !current_target {
            // Stale failures must not reconnect the active session.
            return vec![];
        }
        // 错误占位 + 可读提示（AC-004-06）。
        let hint = format!("{code}: {message}");
        self.image_errors
            .insert(attachment_id.clone(), hint.clone());
        if !for_viewer
            && self.mode == Mode::ImageView
            && self.image_view.block_seq == Some(block_seq)
        {
            self.image_view.mark_failed(code.clone(), message.clone());
        }
        if retryable {
            // 断网走既有指数退避重连（AC-004-06；恢复后可重开）。
            return self.handle_disconnected(format!("attachment 拉取网络错误: {code} {message}"));
        }
        // 权限/格式类不自动重试（AC-004-06）。
        vec![]
    }

    /// 进程退出清理：删除未入缓存的临时文件（06 §9；缓存目录由
    /// ImageCache Drop 清理）。
    pub fn cleanup_transient_files(&mut self) {
        if let Some(id) = self.image_view.attachment_id.clone() {
            self.image_cache.unpin(&id);
        }
        for path in self.transient_files.drain(..) {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::debug!(path = %path.display(), error = %e, "临时文件清理失败（忽略）");
            }
        }
    }

    /// 注入 config 的 `cache_bytes` 预算（main 启动时调用一次）。
    pub fn set_cache_budget(&mut self, budget: u64) {
        self.image_cache.set_budget(budget);
    }

    /// kitty image id 分配（在 u8 空间循环，避免饱和后永久复用 255）。
    pub fn next_kitty_frame_id(&mut self) -> u8 {
        self.kitty_frame_id = if self.kitty_frame_id >= u16::from(u8::MAX) {
            1
        } else {
            self.kitty_frame_id + 1
        };
        self.kitty_frame_id as u8
    }

    // ---------- input commands (Step 5 keymap maps here) ----------

    pub fn handle_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        // Any key other than Quit cancels a pending quit confirmation.
        if !matches!(&cmd, C::Quit) {
            self.quit_requested = false;
        }
        match cmd {
            C::MoveDown
            | C::MoveUp
            | C::HalfPageDown
            | C::HalfPageUp
            | C::GotoBottom
            | C::GotoTop => {
                if self.mode == Mode::Visual {
                    // VISUAL：j/k 扩展选择（V 行模式跨块；v 字符模式单块）。
                    let len = self.active_window().map(|w| w.len()).unwrap_or(0);
                    if let Some(sel) = self.yank.visual.as_mut() {
                        if matches!(cmd, C::MoveDown) {
                            sel.cursor = (sel.cursor + 1).min(len.saturating_sub(1));
                        } else if matches!(cmd, C::MoveUp) {
                            sel.cursor = sel.cursor.saturating_sub(1);
                        }
                    }
                    vec![]
                } else if self.outline.open {
                    // 大纲列表：j/k 选择。
                    let total = self
                        .active_window()
                        .map(|w| w.turn_outline().len())
                        .unwrap_or(0);
                    match cmd {
                        C::MoveDown => {
                            self.outline.selection =
                                (self.outline.selection + 1).min(total.saturating_sub(1));
                        }
                        C::MoveUp => {
                            self.outline.selection = self.outline.selection.saturating_sub(1);
                        }
                        _ => {}
                    }
                    vec![]
                } else {
                    self.scroll(cmd)
                }
            }
            C::OpenPicker => {
                self.mode = Mode::Picker;
                self.picker.open = true;
                self.picker.query.clear();
                self.picker.selection = 0;
                vec![]
            }
            C::InsertMode => self.open_composer(),
            C::ClosePicker => match self.mode {
                Mode::Picker => {
                    self.mode = Mode::Normal;
                    self.picker.open = false;
                    vec![]
                }
                // Esc closes the composer but keeps the session draft
                // (AC-002-04).
                Mode::Insert => {
                    self.close_composer_keep_draft();
                    vec![]
                }
                Mode::Search => {
                    self.close_search();
                    vec![]
                }
                Mode::Visual => {
                    // Esc/v/V 退出视觉选择（Notes/04 §3.1）。
                    self.yank.visual = None;
                    self.mode = Mode::Normal;
                    vec![]
                }
                Mode::Approval => {
                    // Esc 在审批弹窗 = cancelled 决策（不退出程序）。
                    self.approval_decide(ApprovalOutcome::Cancelled)
                }
                Mode::Normal => {
                    if self.outline.open {
                        self.outline.open = false;
                        self.outline.selection = 0;
                    }
                    vec![]
                }
                // IMAGEVIEW：Esc 无语义（`q` 关闭，D-14）。
                Mode::ImageView => vec![],
            },
            C::PickerDown => {
                if self.mode == Mode::Search {
                    if !self.search.results_locked {
                        // 编辑段：j 是输入字符。
                        return self.search_input("j");
                    }
                    let total = self.search.history_hits.len();
                    if total > 0 {
                        self.search.history_selection =
                            (self.search.history_selection + 1).min(total - 1);
                    }
                } else {
                    self.picker.selection += 1;
                }
                vec![]
            }
            C::PickerUp => {
                if self.mode == Mode::Search {
                    if !self.search.results_locked {
                        // 编辑段：k 是输入字符。
                        return self.search_input("k");
                    }
                    self.search.history_selection = self.search.history_selection.saturating_sub(1);
                } else {
                    self.picker.selection = self.picker.selection.saturating_sub(1);
                }
                vec![]
            }
            C::PickerInput(text) => {
                if self.mode == Mode::Insert {
                    self.composer_input(&text)
                } else if self.mode == Mode::Search {
                    self.search_input(&text)
                } else {
                    self.picker.query.push_str(&text);
                    self.picker.selection = 0;
                    vec![]
                }
            }
            C::PickerBackspace => {
                if self.mode == Mode::Insert {
                    self.composer_backspace()
                } else if self.mode == Mode::Search {
                    self.search_input_backspace()
                } else {
                    self.picker.query.pop();
                    vec![]
                }
            }
            C::SubmitInput => {
                if self.mode == Mode::Insert {
                    // 运行中 → steer（mode 按官方 projection 判定，AC-003-06）。
                    let prompt_mode = if self.active_running() {
                        PromptMode::Steer
                    } else {
                        PromptMode::Queue
                    };
                    self.submit_input(prompt_mode)
                } else {
                    vec![]
                }
            }
            C::HistoryPrev => {
                if self.mode == Mode::Insert {
                    self.composer_history_prev()
                } else {
                    vec![]
                }
            }
            C::HistoryNext => {
                if self.mode == Mode::Insert {
                    self.composer_history_next()
                } else {
                    vec![]
                }
            }
            C::StartSearch => {
                if self.mode == Mode::Normal {
                    self.open_search();
                }
                vec![]
            }
            // SEARCH 两段式：编辑段（未锁定）n/N/y/j/k 是输入字符；Enter 后
            // 结果巡览段（锁定）才是命令（AC-003-05 与「输入实时过滤」兼容）。
            C::SearchNext => {
                if self.mode == Mode::Search && !self.search.results_locked {
                    self.search_input("n")
                } else {
                    self.search_next(1)
                }
            }
            C::SearchPrev => {
                if self.mode == Mode::Search && !self.search.results_locked {
                    self.search_input("N")
                } else {
                    self.search_next(-1)
                }
            }
            C::VisualStart { line } => {
                if self.mode == Mode::Normal {
                    self.start_visual(line);
                }
                vec![]
            }
            C::VisualYank => self.visual_yank(),
            C::YankContext => match self.mode {
                Mode::Visual => self.visual_yank(),
                // SEARCH 中 `y` 复制当前命中（代码块 → 整块，AC-003-03）；
                // 编辑段先作为输入字符。
                Mode::Search if !self.search.results_locked => self.search_input("y"),
                Mode::Search => self.search_yank(),
                _ => self.context_yank(),
            },
            C::OpenOutline => {
                if self.mode == Mode::Normal {
                    self.outline.open = true;
                    self.outline.selection = 0;
                }
                vec![]
            }
            C::NextTurn => self.jump_turn(1),
            C::PrevTurn => self.jump_turn(-1),
            C::OpenSelected => {
                // Center 焦点两级语义：图片占位 → REQ-004 打开图片；
                // 否则 `o` 打开光标处链接（D-19，AC-003-04/20）。
                if self.focus == Focus::Center && self.focused_image_block().is_some() {
                    self.open_focused_image()
                } else {
                    match self.focus {
                        Focus::Sidebar => self.open_session_from_selection(),
                        _ => self.open_external_at_cursor(),
                    }
                }
            }
            C::OpenFocused => {
                // NORMAL Enter：仅中心区图片焦点打开图片（D-14）。
                if self.focus == Focus::Center && self.focused_image_block().is_some() {
                    self.open_focused_image()
                } else {
                    vec![]
                }
            }
            C::ImageViewClose => {
                // AC-004-05 `q`：关闭 ImageView 回 transcript（NORMAL）。
                if self.mode == Mode::ImageView {
                    if let Some(id) = self.image_view.attachment_id.clone() {
                        self.image_cache.unpin(&id);
                    }
                    self.mode = Mode::Normal;
                    self.image_view.close();
                    self.image_frame = None;
                    // 未入缓存且不再展示：立即回收临时文件。
                    if let Some(path) = self.view_temp_path.take() {
                        if let Some(pos) = self.transient_files.iter().position(|p| *p == path) {
                            self.transient_files.swap_remove(pos);
                        }
                        let _ = std::fs::remove_file(&path);
                    }
                }
                vec![]
            }
            C::ImageViewCopy => {
                // AC-004-05 `y`：复制图片路径/附件名（arboard 在 main 执行，
                // 不可用降级提示不崩溃）。
                if self.mode != Mode::ImageView {
                    return vec![];
                }
                let text = self
                    .view_image_path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .or_else(|| self.image_view.name.clone())
                    .or_else(|| self.image_view.attachment_id.as_ref().map(|a| a.0.clone()));
                match text {
                    Some(t) => vec![Cmd::CopyImageText { text: t }],
                    None => vec![],
                }
            }
            C::ImageViewOpenExternal => {
                // AC-004-05 `o`：系统查看器打开原图（不阻塞，main 执行）。
                if self.mode != Mode::ImageView {
                    return vec![];
                }
                match self.view_image_path() {
                    Some(path) => vec![Cmd::OpenSystemViewer { path }],
                    None => {
                        self.last_error = Some("图片尚未就绪，暂不能打开系统查看器".into());
                        vec![]
                    }
                }
            }
            C::StopRunning => self.request_stop(),
            C::CollapseProject => {
                for workspace in &self.workspaces.workspaces {
                    self.collapsed_workspaces.insert(workspace.id.clone());
                }
                vec![]
            }
            C::ExpandProject => {
                self.collapsed_workspaces.clear();
                vec![]
            }
            C::PickerConfirm => match self.mode {
                Mode::Picker => {
                    let chosen = self.picker_selected_session();
                    self.mode = Mode::Normal;
                    self.picker.open = false;
                    match chosen {
                        Some(sid) => self.open_session(sid),
                        None => vec![],
                    }
                }
                Mode::Search => self.search_confirm(),
                // APPROVAL 仅 y/n/q/Esc/a（§3 键位边界；Enter 无语义，no-op）。
                Mode::Normal if self.outline.open => self.outline_confirm(),
                _ => vec![],
            },
            C::ApprovalAllow => self.approval_decide(ApprovalOutcome::AllowedOnce),
            C::ApprovalReject => self.approval_decide(ApprovalOutcome::Rejected),
            C::ApprovalCancel => self.approval_decide(ApprovalOutcome::Cancelled),
            C::ApprovalAlways => {
                // `a` 非 outcome 词表：TUI 不代远端切换 approval/policy=never
                // （REQ-I04 V0.4），只显示指引（D-18）。
                self.approval.toast = Some(
                    "始终允许需在官方 web 策略设置中切换（approval/policy=never，V0.4）"
                        .to_string(),
                );
                vec![]
            }
            C::OpenSession(sid) => self.open_session(sid),
            C::OpenHelp => {
                self.help_open = true;
                vec![]
            }
            C::CloseHelp => {
                self.help_open = false;
                vec![]
            }
            C::CycleFocus => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Center,
                    Focus::Center => Focus::Details,
                    Focus::Details => Focus::Sidebar,
                };
                vec![]
            }
            C::ToggleWorkspace => vec![],
            C::Quit => {
                if self.mode == Mode::Approval {
                    // APPROVAL 中 `q` = 中止当前审批（cancelled），不退出
                    // （REQ-003 §3 `q` 键位边界）。
                    self.approval_decide(ApprovalOutcome::Cancelled)
                } else {
                    self.quit()
                }
            }
            C::RetryProbe => {
                self.conn = ConnState::Connecting;
                self.startup_guidance = None;
                vec![]
            }
            C::Resize { width, height } => self.handle(AppEvent::Resize { width, height }),
        }
    }

    // ---------- REQ-003: search / visual / approval / outline helpers ----------

    fn open_search(&mut self) {
        self.mode = Mode::Search;
        self.search.open = true;
        self.search.query.clear();
        self.search.window_matches.clear();
        self.search.cursor = 0;
        self.search.history_hits.clear();
        self.search.history_selection = 0;
        self.search.history_error = None;
        self.search.history_hint = None;
        self.search.results_locked = false;
        // 打开即作废在途搜索（generation 递增 → 旧触发/结果全部丢弃）。
        self.search.history_generation = self.search.history_generation.wrapping_add(1);
    }

    fn close_search(&mut self) {
        self.mode = Mode::Normal;
        self.search.open = false;
        self.search.history_generation = self.search.history_generation.wrapping_add(1);
        self.search.history_loading = false;
        self.search.results_locked = false;
    }

    fn search_input(&mut self, text: &str) -> Vec<Cmd> {
        self.search.query.push_str(text);
        // 输入变更 → 回到编辑段（n/N/y/j/k 恢复为字符输入）。
        self.search.results_locked = false;
        self.recompute_window_matches();
        self.schedule_history_search()
    }

    fn search_input_backspace(&mut self) -> Vec<Cmd> {
        self.search.query.pop();
        self.search.results_locked = false;
        self.recompute_window_matches();
        self.schedule_history_search()
    }

    /// 300ms 防抖 + generation 守卫（AC-003-14：快速连续输入只保留最新；
    /// 空查询/纯前缀不发 `session/search`，AC-003-19）。
    fn schedule_history_search(&mut self) -> Vec<Cmd> {
        let (_, term) = self.search.terms();
        let term = term.trim().to_string();
        if term.is_empty() {
            self.search.history_generation = self.search.history_generation.wrapping_add(1);
            self.search.history_loading = false;
            return vec![];
        }
        self.search.history_generation = self.search.history_generation.wrapping_add(1);
        let generation = self.search.history_generation;
        vec![Cmd::DebounceSearch {
            query: term,
            generation,
        }]
    }

    fn search_next(&mut self, delta: i64) -> Vec<Cmd> {
        if self.mode != Mode::Search || self.search.window_matches.is_empty() {
            return vec![];
        }
        let total = self.search.window_matches.len() as i64;
        let cur = self.search.cursor as i64;
        self.search.cursor = ((cur + delta).rem_euclid(total)) as usize;
        self.jump_to_current_match();
        vec![]
    }

    /// 跳到当前窗口命中所在块（滚动视口 + 焦点块游标）。
    fn jump_to_current_match(&mut self) {
        let Some(item_index) = self.search.window_matches.get(self.search.cursor).copied() else {
            return;
        };
        let Some(item) = self.search_index.items().get(item_index) else {
            return;
        };
        let Some(idx) = self.active_window().and_then(|w| w.offset_of(item.seq)) else {
            return;
        };
        self.viewport.follow_tail = false;
        self.viewport.offset = idx;
        self.cursor_block = idx;
    }

    /// Enter 语义（D-17/AC-003-03）：历史命中选中项优先 → 打开命中会话，
    /// 并以 snippet 作为窗口内二次定位词（快照到达后 window_changed 自动
    /// 重算命中并高亮）；否则跳到窗口首个匹配。Enter 同时锁定结果巡览段
    /// （n/N/y/j/k 恢复为命令，AC-003-05）。
    fn search_confirm(&mut self) -> Vec<Cmd> {
        self.search.results_locked = true;
        if let Some(hit) = self.search.history_hits.get(self.search.history_selection) {
            let sid = hit.session_id.clone();
            // snippet 截取为可检索词：去掉截断省略号与首尾空白（服务端
            // ≤240 码点截断后缀 "…"，模糊匹配需按原文词面）。
            let snippet = hit
                .snippet
                .trim()
                .trim_matches(|c: char| c == '…' || c == '.')
                .trim()
                .to_string();
            let cmds = self.open_session(sid);
            // 重新打开窗口内搜索 overlay：snippet 二次定位（D-17 收缩）。
            self.open_search();
            self.search.query = snippet.clone();
            self.recompute_window_matches();
            // 全历史词仍按原查询保留（不重复触发 session/search）。
            return cmds;
        }
        if let Some(item_index) = self.search.window_matches.first().copied() {
            let item = &self.search_index.items()[item_index];
            if let Some(idx) = self.active_window().and_then(|w| w.offset_of(item.seq)) {
                self.viewport.follow_tail = false;
                self.viewport.offset = idx;
                self.cursor_block = idx;
                self.search.cursor = 0;
            }
        }
        vec![]
    }

    fn start_visual(&mut self, line: bool) {
        let len = self.active_window().map(|w| w.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        self.cursor_block = self.cursor_block.min(len - 1);
        self.yank.visual = Some(VisualSelection {
            anchor: self.cursor_block,
            cursor: self.cursor_block,
            mode: if line {
                VisualMode::Line
            } else {
                VisualMode::Char
            },
        });
        self.mode = Mode::Visual;
    }

    /// VISUAL `y`：按选择复制并退出（AC-003-12）。
    fn visual_yank(&mut self) -> Vec<Cmd> {
        if self.mode != Mode::Visual {
            return vec![];
        }
        let Some(sel) = self.yank.visual.clone() else {
            return vec![];
        };
        let blocks = self
            .active_window()
            .map(|w| w.block_snapshot())
            .unwrap_or_default();
        let Some(text) = selection_text(&blocks, &sel) else {
            self.yank.visual = None;
            self.mode = Mode::Normal;
            return vec![];
        };
        self.yank.visual = None;
        self.mode = Mode::Normal;
        self.copy_text(text)
    }

    /// NORMAL `y`：上下文 yank（代码块/链接/图片/工具结果/段落，Notes/04 §4.4）。
    fn context_yank(&mut self) -> Vec<Cmd> {
        let Some(block) = self
            .active_window()
            .and_then(|w| w.block(self.cursor_block))
        else {
            return vec![];
        };
        let Some(target) = block_yank_target(block) else {
            self.last_error = Some("此处无可复制内容".to_string());
            return vec![];
        };
        self.copy_text(target.content().to_string())
    }

    /// SEARCH 模式 `y`：复制当前窗口命中（代码块 → 整块，AC-003-03）。
    fn search_yank(&mut self) -> Vec<Cmd> {
        let Some(item_index) = self.search.window_matches.get(self.search.cursor).copied() else {
            return vec![];
        };
        let Some(item) = self.search_index.items().get(item_index) else {
            return vec![];
        };
        self.copy_text(item.text.clone())
    }

    fn copy_text(&mut self, text: String) -> Vec<Cmd> {
        self.yank.last = Some(text.clone());
        vec![Cmd::CopyToClipboard { text }]
    }

    /// `o`/`Enter`（Center 焦点）：光标块含 URL → 系统打开；否则 no-op 提示
    /// （AC-003-04/20）。
    fn open_external_at_cursor(&mut self) -> Vec<Cmd> {
        let Some(block) = self
            .active_window()
            .and_then(|w| w.block(self.cursor_block))
        else {
            return vec![];
        };
        let text = block_plain_text(block);
        if let Some((_, url)) = crate::model::search::extract_links(&text)
            .into_iter()
            .next()
        {
            return vec![Cmd::OpenExternal { target: url }];
        }
        self.last_error = Some("光标处没有链接（o 打开仅对链接生效）".to_string());
        vec![]
    }

    /// 审批决策（y/n/q 共用；同一弹窗只回复一次 — 幂等，AC-003-17）。
    fn approval_decide(&mut self, outcome: ApprovalOutcome) -> Vec<Cmd> {
        if self.mode != Mode::Approval || self.approval.reply_inflight {
            return vec![];
        }
        let Some(event) = self.approval.event.clone() else {
            return vec![];
        };
        self.approval.reply_inflight = true;
        vec![Cmd::ReplyApproval { event, outcome }]
    }

    fn outline_confirm(&mut self) -> Vec<Cmd> {
        let Some(item) = self
            .active_window()
            .and_then(|w| w.turn_outline().get(self.outline.selection).cloned())
        else {
            return vec![];
        };
        let Some(seq) = item.seq else {
            return vec![];
        };
        self.outline.open = false;
        self.outline.selection = 0;
        self.jump_to_seq(seq)
    }

    /// `]`/`[`：turnOutline 下一/上一轮（AC-003-09）。
    fn jump_turn(&mut self, delta: i64) -> Vec<Cmd> {
        let Some(outline) = self.active_window().map(|w| w.turn_outline().to_vec()) else {
            return vec![];
        };
        if outline.is_empty() {
            return vec![];
        }
        let focused_seq = self
            .active_window()
            .and_then(|w| w.block(self.cursor_block))
            .map(|b| b.seq())
            .unwrap_or(SessionSeq(0));
        // 当前轮 = 最后一个 seq <= 焦点 seq 的条目。
        let cur = outline
            .iter()
            .enumerate()
            .filter(|(_, t)| t.seq.is_some_and(|s| s <= focused_seq))
            .map(|(i, _)| i)
            .next_back()
            .unwrap_or(0);
        let total = outline.len() as i64;
        let next = (cur as i64 + delta).clamp(0, total - 1) as usize;
        match outline[next].seq {
            Some(seq) => self.jump_to_seq(seq),
            None => vec![],
        }
    }

    /// 跳转目标 seq：窗口已含 → 直接落位；否则 loadThrough 逐页拉取，
    /// 目标记录在 `load_through_target`（每页合并后 reducer 判断落位）。
    fn jump_to_seq(&mut self, seq: SessionSeq) -> Vec<Cmd> {
        if self.scroll_to_seq(seq) {
            if let Some(idx) = self.active_window().and_then(|w| w.offset_of(seq)) {
                self.cursor_block = idx;
            }
            vec![]
        } else {
            self.load_through_target = Some(seq);
            self.load_through_pages = 0;
            vec![Cmd::LoadThrough { seq }]
        }
    }

    fn composer_history_prev(&mut self) -> Vec<Cmd> {
        let current = self
            .draft
            .as_ref()
            .map(|d| d.text.clone())
            .unwrap_or_default();
        if let Some(text) = self.history.prev(&current) {
            let text = text.to_string();
            if let Some(d) = self.draft.as_mut() {
                d.text = text.clone();
                d.cursor = text.chars().count();
            }
        }
        vec![]
    }

    fn composer_history_next(&mut self) -> Vec<Cmd> {
        if let Some(text) = self.history.next_entry() {
            let text = text.to_string();
            if let Some(d) = self.draft.as_mut() {
                d.text = text.clone();
                d.cursor = text.chars().count();
            }
        }
        vec![]
    }

    fn scroll(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        let len = self.active_window().map(|w| w.len()).unwrap_or(0);
        let height = self.viewport.height.max(1);
        let mut cmds = Vec::new();
        match cmd {
            C::MoveDown => {
                self.viewport.follow_tail = false;
                self.viewport.offset = (self.viewport.offset + 1).min(len.saturating_sub(height));
            }
            C::MoveUp => {
                self.viewport.follow_tail = false;
                self.viewport.offset = self.viewport.offset.saturating_sub(1);
                cmds = self.maybe_request_page();
            }
            C::HalfPageDown => {
                self.viewport.follow_tail = false;
                self.viewport.offset =
                    (self.viewport.offset + height / 2).min(len.saturating_sub(height));
            }
            C::HalfPageUp => {
                self.viewport.follow_tail = false;
                self.viewport.offset = self.viewport.offset.saturating_sub(height / 2);
                cmds = self.maybe_request_page();
            }
            C::GotoBottom => {
                self.viewport.follow_tail = true;
                self.scroll_to_bottom();
            }
            C::GotoTop => {
                self.viewport.follow_tail = false;
                self.viewport.offset = 0;
                cmds = self.maybe_request_page();
            }
            _ => {}
        }
        // 焦点块游标随视口移动（上下文 yank/视觉选择/跳轮的锚）。
        self.cursor_block = self.viewport.offset.min(len.saturating_sub(1));
        self.refresh_focus();
        cmds
    }

    /// At the top with earlier history available → send a page request
    /// (single-flight + Ready only + disconnect just records want_backfill).
    fn maybe_request_page(&mut self) -> Vec<Cmd> {
        let at_top = self.viewport.offset == 0;
        let has_more = self
            .active_window()
            .map(|w| w.head_has_more())
            .unwrap_or(false);
        if at_top && has_more && self.conn == ConnState::Ready && !self.page_guard.in_flight {
            self.want_backfill = true;
            vec![self.page_cmd()]
        } else {
            if at_top && has_more {
                self.want_backfill = true;
            }
            vec![]
        }
    }

    fn open_session(&mut self, sid: SessionId) -> Vec<Cmd> {
        // 会话切换：把编辑中的草稿存入注册表（D-20 跨会话保留，仅内存）。
        if let Some(d) = self.draft.take() {
            self.drafts.set(d);
        }
        if let Some(old_id) = self.image_view.attachment_id.clone() {
            self.image_cache.unpin(&old_id);
        }
        self.image_view.close();
        self.image_frame = None;
        self.view_temp_path = None;
        self.active_session = Some(sid.clone());
        self.viewport.follow_tail = true;
        self.cursor_block = 0;
        self.composer.visible = false;
        self.composer.steer = false;
        self.composer.active_session = Some(sid.clone());
        // 关掉内容 overlay（搜索/大纲），回到 NORMAL 内容浏览态。
        if self.mode == Mode::Search {
            self.close_search();
        }
        self.outline.open = false;
        self.yank.visual = None;
        self.search_index.rebuild(&[]);
        self.search.window_matches.clear();
        // REQ-003：打开会话同时订阅 control 流（运行/steer 瞬态投影）。
        vec![
            Cmd::OpenFollow {
                session_id: sid.clone(),
                max_messages: self.window_cap,
            },
            Cmd::OpenControl { session_id: sid },
        ]
    }

    fn open_session_from_selection(&mut self) -> Vec<Cmd> {
        match self.picker_selected_session() {
            Some(sid) => self.open_session(sid),
            None => vec![],
        }
    }

    fn picker_selected_session(&self) -> Option<SessionId> {
        // Same nucleo matcher seam as the picker UI (ADR-003): selection always
        // refers to the filtered list the user actually sees.
        self.workspaces
            .match_sessions(&self.picker.query)
            .into_iter()
            .nth(self.picker.selection)
            .map(|meta| meta.id.clone())
    }

    /// AC-001-08 / FR-001-07: `q` exits directly; a running session requests
    /// stop first. The first Quit while running only shows a confirmation
    /// hint (stop first, then confirm exit); the second performs cancel →
    /// terminal restore → exit.
    pub fn quit(&mut self) -> Vec<Cmd> {
        if self.active_running() && !self.quit_requested {
            self.quit_requested = true;
            self.last_error =
                Some("运行中会话：再按一次 q 或 Ctrl+c 确认退出（将先请求 stop）".to_string());
            return vec![];
        }
        self.quit_requested = true;
        let mut cmds = Vec::new();
        if let Some(sid) = self.active_session.clone() {
            let stopping = self.stop.is_requested_for(&sid);
            // Running or stopping (local transition): best-effort stop first.
            if self.running_sessions.contains(&sid) || stopping {
                cmds.push(Cmd::CancelSession(sid));
            }
        }
        cmds.push(Cmd::RestoreTerminal);
        cmds.push(Cmd::Exit);
        self.exited = true;
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Command as C;

    fn snapshot(sid: &str, running: bool) -> AppEvent {
        AppEvent::FollowSnapshot {
            session_id: SessionId(sid.into()),
            cursor: Some(SessionLogOffset(10)),
            records: (1..=3)
                .map(|s| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "user/message".into(),
                        seq: Some(SessionSeq(s)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: None,
                    },
                })
                .collect(),
            has_more: true,
            projections: Some(serde_json::json!({"running": running, "title": "t"})),
        }
    }

    #[test]
    fn startup_loads_list_and_workspace() {
        let mut s = AppState::default();
        let cmds = s.handle(AppEvent::Startup);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Cmd::LoadSessionList { .. })));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::OpenWorkspaceFollow)));
    }

    #[test]
    fn kitty_frame_id_wraps_from_255_to_1() {
        // AC-004-09：kitty image id 在 u8 空间循环（255 → 1），避免饱和后
        // 永久复用同一 id。
        let mut s = AppState {
            kitty_frame_id: 254,
            ..AppState::default()
        };
        assert_eq!(s.next_kitty_frame_id(), 255);
        assert_eq!(s.next_kitty_frame_id(), 1);
        assert_eq!(s.next_kitty_frame_id(), 2);
    }

    #[test]
    fn startup_probe_failed_shows_guidance_ac001_02() {
        let mut s = AppState::default();
        let cmds = s.handle(AppEvent::StartupProbeFailed("连接被拒绝".into()));
        assert!(cmds.is_empty(), "探针失败不拉起后端进程");
        assert_eq!(s.conn, ConnState::StartupFailed);
        let g = s.guidance_text();
        assert!(g.contains("请启动 dsh web"), "guidance={g}");
        assert!(g.contains("127.0.0.1:3080"), "guidance={g}");
        assert!(g.contains("[r] 重试"), "guidance={g}");
        // Retry path: RetryProbe returns to Connecting and clears the guidance
        // (recovery not polluted by old state).
        s.handle_command(C::RetryProbe);
        assert_eq!(s.conn, ConnState::Connecting);
        assert!(s.startup_guidance.is_none());
    }

    #[test]
    fn page_single_flight_and_generation() {
        let mut s = AppState::default();
        s.handle(AppEvent::Startup);
        s.handle(AppEvent::SessionListPage {
            items: vec![],
            next_cursor: None,
        });
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        let cmds = s.handle_command(C::GotoTop);
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::RequestPage { .. }))
                .count(),
            1
        );
        let gen = match &cmds[0] {
            Cmd::RequestPage { generation, .. } => *generation,
            _ => panic!("预期 page 命令"),
        };
        // Hit the top again while in flight → no new request.
        assert!(s.handle_command(C::GotoTop).is_empty());
        // A stale response is dropped.
        let cmds = s.handle(AppEvent::PageResult {
            session_id: SessionId("s1".into()),
            generation: gen + 99,
            records: vec![],
            has_more: None,
        });
        assert!(cmds.is_empty());
        // Normal completion releases the single-flight slot.
        s.handle(AppEvent::PageResult {
            session_id: SessionId("s1".into()),
            generation: gen,
            records: vec![],
            has_more: Some(true),
        });
        let cmds = s.handle_command(C::GotoTop);
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::RequestPage { .. }))
                .count(),
            1,
            "完成后可再次发起（generation 递增）"
        );
    }

    #[test]
    fn disconnected_shows_reconnecting_and_single_refollow() {
        let mut s = AppState::default();
        s.handle(AppEvent::Startup);
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        let cmds = s.handle(AppEvent::Disconnected("eof".into()));
        assert!(s.is_reconnecting());
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Reconnect { .. })));
        // While disconnected, scrolling never sends an HTTP page request.
        let cmds = s.handle_command(C::GotoTop);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::RequestPage { .. })));
        // Recovery → triggers exactly one refollow.
        let cmds = s.handle(AppEvent::Reconnected);
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::OpenFollow { .. }))
                .count(),
            1
        );
        assert_eq!(s.conn, ConnState::Ready);
        // refollow snapshot arrives → send the backfill (AC-001-12
        // reconciliation).
        let cmds = s.handle(snapshot("s1", false));
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::RequestPage { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn permission_error_does_not_reconnect() {
        let mut s = AppState::default();
        s.handle(AppEvent::Startup);
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        let cmds = s.handle(AppEvent::FollowError {
            session_id: SessionId("s1".into()),
            error: ClientError::Stream {
                code: "PERMISSION_DENIED".into(),
                message: "无权限".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Reconnect { .. })));
        assert!(!s.is_reconnecting(), "权限错误不触发重连");
        assert!(s.last_error.as_deref().unwrap_or("").contains("权限不足"));
    }

    #[test]
    fn quit_running_session_order_cancel_restore_exit() {
        let mut s = AppState::default();
        s.handle(AppEvent::Startup);
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true));
        // Running: the first quit asks for confirmation (FR-001-07) ...
        let cmds = s.handle_command(C::Quit);
        assert!(cmds.is_empty(), "first quit only confirms: {cmds:?}");
        assert!(s
            .last_error
            .as_deref()
            .is_some_and(|m| m.contains("确认退出")));
        assert!(!s.exited);
        // ... the second quit stops first, then restores, then exits.
        let cmds = s.handle_command(C::Quit);
        assert_eq!(
            cmds,
            vec![
                Cmd::CancelSession(SessionId("s1".into())),
                Cmd::RestoreTerminal,
                Cmd::Exit,
            ]
        );
        // Not running: no cancel, no confirmation.
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s2".into())));
        s.handle(snapshot("s2", false));
        let cmds = s.handle_command(C::Quit);
        assert_eq!(cmds, vec![Cmd::RestoreTerminal, Cmd::Exit]);
    }

    #[test]
    fn follow_event_tail_follow_and_browse_freeze() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.viewport.height = 2;
        // follow_tail: appended events stay stuck to the tail.
        for i in 4..=6 {
            s.handle(AppEvent::FollowEvent {
                session_id: SessionId("s1".into()),
                event: SessionWireEvent {
                    event_type: "assistant/message".into(),
                    seq: Some(SessionSeq(i)),
                    time: None,
                    request_id: None,
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                    data: None,
                },
            });
        }
        assert!(s.viewport.follow_tail);
        // Scrolling up freezes the tail.
        s.handle_command(C::GotoTop);
        assert!(!s.viewport.follow_tail);
        assert_eq!(s.viewport.offset, 0);
        // Appended events do not move the browsing position (anchor stable).
        s.handle(AppEvent::FollowEvent {
            session_id: SessionId("s1".into()),
            event: SessionWireEvent {
                event_type: "assistant/message".into(),
                seq: Some(SessionSeq(50)),
                time: None,
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: None,
            },
        });
        assert_eq!(s.viewport.offset, 0, "浏览中尾部追加不打扰视口");
    }

    #[test]
    fn stale_page_error_dropped() {
        let mut s = AppState::default();
        s.handle(AppEvent::Startup);
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        let cmds = s.handle_command(C::GotoTop);
        let gen = match &cmds[0] {
            Cmd::RequestPage { generation, .. } => *generation,
            _ => 0,
        };
        // An error from an old generation must not affect the current
        // single-flight request.
        let cmds = s.handle(AppEvent::PageError {
            session_id: SessionId("s1".into()),
            generation: gen - 1,
            error: ClientError::Transport("x".into()),
        });
        assert!(cmds.is_empty());
        assert!(s.page_guard.in_flight, "stale 错误不得释放单飞");
    }

    #[test]
    fn resize_updates_viewport_height() {
        let mut s = AppState::default();
        s.handle(AppEvent::Resize {
            width: 120,
            height: 40,
        });
        assert_eq!((s.width, s.height), (120, 40));
        assert_eq!(s.viewport.height, 38);
    }

    // ---------- REQ-002 composer 生命周期（Step 2） ----------

    #[test]
    fn insert_requires_active_session_ac002_12() {
        let mut s = AppState::default();
        s.handle(AppEvent::Startup);
        s.handle(AppEvent::SessionListPage {
            items: vec![],
            next_cursor: None,
        });
        let cmds = s.handle_command(C::InsertMode);
        assert!(cmds.is_empty());
        assert_eq!(s.mode, Mode::Normal, "无活动会话不进入 INSERT");
        assert!(!s.composer.visible);
        assert!(
            s.last_error
                .as_deref()
                .unwrap_or("")
                .contains("无打开的会话"),
            "AC-002-12 状态条提示"
        );
    }

    #[test]
    fn esc_keeps_draft_per_session_and_switch_does_not_restore_ac002_04() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle_command(C::InsertMode);
        assert_eq!(s.mode, Mode::Insert);
        assert!(s.composer.visible);
        s.handle_command(C::PickerInput("你好".into()));
        // Esc 收起但保留草稿（会话内）。
        s.handle_command(C::ClosePicker);
        assert_eq!(s.mode, Mode::Normal);
        assert!(!s.composer.visible);
        assert_eq!(s.draft.as_ref().map(|d| d.text.as_str()), Some("你好"));
        // 同一会话再次 i：草稿还在。
        s.handle_command(C::InsertMode);
        assert!(s.composer.visible);
        assert_eq!(s.draft.as_ref().map(|d| d.text.as_str()), Some("你好"));
        // 切换会话：新会话草稿为空（跨会话不恢复，D-11）。
        s.handle_command(C::ClosePicker);
        s.handle_command(C::OpenSession(SessionId("s2".into())));
        s.handle(snapshot("s2", false));
        s.handle_command(C::InsertMode);
        assert_eq!(
            s.draft.as_ref().map(|d| d.text.as_str()),
            Some(""),
            "跨会话不恢复草稿"
        );
        assert_eq!(
            s.draft.as_ref().map(|d| d.bound_session.0.as_str()),
            Some("s2")
        );
    }

    #[test]
    fn empty_enter_stays_insert_and_sends_nothing_ac002_03() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle_command(C::InsertMode);
        let cmds = s.handle_command(C::SubmitInput);
        assert!(cmds.is_empty(), "空输入不发");
        assert_eq!(s.mode, Mode::Insert, "空输入保持 INSERT");
        assert!(s.composer.visible);
        // 纯空白同样视为空输入。
        s.handle_command(C::PickerInput("   ".into()));
        let cmds = s.handle_command(C::SubmitInput);
        assert!(cmds.is_empty(), "纯空白不发");
        assert_eq!(s.mode, Mode::Insert);
    }

    #[test]
    fn nonempty_enter_closes_composer_back_to_normal_ac002_01() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle_command(C::InsertMode);
        s.handle_command(C::PickerInput("hi".into()));
        let cmds = s.handle_command(C::SubmitInput);
        assert_eq!(s.mode, Mode::Normal, "Enter 后自动收起");
        assert!(!s.composer.visible);
        assert_eq!(
            s.draft.as_ref().map(|d| d.text.as_str()),
            Some(""),
            "发送后草稿清空"
        );
        assert_eq!(cmds.len(), 1, "非空 Enter 恰好一个发送命令");
        assert!(matches!(cmds[0], Cmd::SendPrompt { .. }));
    }

    // ---------- REQ-002 发送入口、乐观回显与 requestId 对账（Step 3） ----------

    /// 空记录快照（不含 seed 记录，便于断言 pending/durable 计数）。
    fn empty_snapshot(sid: &str) -> AppEvent {
        AppEvent::FollowSnapshot {
            session_id: SessionId(sid.into()),
            cursor: Some(SessionLogOffset(0)),
            records: vec![],
            has_more: true,
            projections: Some(serde_json::json!({"running": false})),
        }
    }

    fn submit_flow(s: &mut AppState, sid: &str, text: &str) -> Vec<Cmd> {
        s.handle_command(C::OpenSession(SessionId(sid.into())));
        s.handle(empty_snapshot(sid));
        s.handle_command(C::InsertMode);
        s.handle_command(C::PickerInput(text.into()));
        s.handle_command(C::SubmitInput)
    }

    #[test]
    fn submit_emits_exactly_one_send_prompt_with_echo_ac002_02() {
        let mut s = AppState::default();
        let cmds = submit_flow(&mut s, "s1", "你好");
        assert_eq!(cmds.len(), 1, "同一 Enter 只产生一次 session/prompt");
        let cmd = &cmds[0];
        let Cmd::SendPrompt {
            session_id,
            request,
        } = cmd
        else {
            panic!("预期 SendPrompt，得到 {cmd:?}")
        };
        assert_eq!(session_id, &SessionId("s1".into()));
        assert_eq!(request.mode, PromptMode::Queue, "V0.1 恒 queue（D-10）");
        assert_eq!(request.session_id, SessionId("s1".into()));
        assert_eq!(request.content.len(), 1);
        let crate::api::types::PromptContentPart::Text { text } = &request.content[0] else {
            panic!("V0.1 仅文本内容")
        };
        assert_eq!(text, "你好");
        assert!(request.client_time_zone.is_none(), "V0.1 不上送时区");
        // 本地立即回显（1 帧内可见，不占 seq）。
        let w = s.sessions.get("s1").unwrap();
        assert_eq!(w.pending().count(), 1);
        assert_eq!(w.pending().next().unwrap().text, "你好");
        assert_eq!(w.len(), 0, "pending 不占 durable blocks");
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn rapid_double_enter_single_send_ac002_13() {
        let mut s = AppState::default();
        let cmds = submit_flow(&mut s, "s1", "quick");
        assert_eq!(cmds.len(), 1);
        let rid = match &cmds[0] {
            Cmd::SendPrompt { request, .. } => request.request_id.0.clone(),
            _ => panic!(),
        };
        // Enter 已触发、composer 已收起：再次提交不产生第二次调用。
        let cmds2 = s.handle_command(C::SubmitInput);
        assert!(cmds2.is_empty());
        // 快速连按（同一帧内第二条 SubmitInput 命令）→ 入口幂等。
        let cmds3 = s.submit_input(PromptMode::Queue);
        assert!(cmds3.is_empty(), "submit 入口幂等");
        let w = s.sessions.get("s1").unwrap();
        assert_eq!(w.pending().count(), 1, "同一 requestId 只回显一次");
        assert_eq!(w.pending().next().unwrap().request_id.0, rid);
    }

    #[test]
    fn prompt_failed_marks_echo_error_and_no_auto_resend_ac002_08() {
        let mut s = AppState::default();
        let cmds = submit_flow(&mut s, "s1", "bad");
        let (session_id, request_id) = match &cmds[0] {
            Cmd::SendPrompt {
                session_id,
                request,
            } => (session_id.clone(), request.request_id.clone()),
            _ => panic!(),
        };
        let out = s.handle(AppEvent::PromptFailed {
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            error: ClientError::Remote {
                code: "gateway/bad-request".into(),
                message: "非法请求".into(),
                class: ErrorClass::UserFacing,
            },
        });
        assert!(out.is_empty(), "失败不自动重发");
        let w = s.sessions.get("s1").unwrap();
        let echo = w.pending().next().unwrap();
        assert!(matches!(
            &echo.status,
            crate::model::PendingEchoStatus::Failed { code, .. }
                if code == "gateway/bad-request"
        ));
        assert!(
            s.last_error.as_deref().unwrap_or("").contains("发送失败"),
            "状态条错误提示"
        );
        // 恢复路径：失败后重新输入可再次手动发送（新 requestId、新回显）。
        let cmds2 = submit_flow(&mut s, "s1", "retry");
        assert_eq!(cmds2.len(), 1, "恢复后可再次手动发送");
        let request_id2 = match &cmds2[0] {
            Cmd::SendPrompt { request, .. } => request.request_id.clone(),
            _ => panic!(),
        };
        assert_ne!(request_id2, request_id, "新请求使用新 requestId");
    }

    #[test]
    fn prompt_permission_denied_no_retry_no_reconnect_ac002_08() {
        let mut s = AppState::default();
        let cmds = submit_flow(&mut s, "s1", "perm");
        let (session_id, request_id) = match &cmds[0] {
            Cmd::SendPrompt {
                session_id,
                request,
            } => (session_id.clone(), request.request_id.clone()),
            _ => panic!(),
        };
        let out = s.handle(AppEvent::PromptFailed {
            session_id,
            request_id,
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "无权限".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(out.is_empty(), "权限错误不自动重试");
        assert!(!s.is_reconnecting(), "权限错误不触发重连");
        let w = s.sessions.get("s1").unwrap();
        let echo = w.pending().next().unwrap();
        assert!(matches!(
            &echo.status,
            crate::model::PendingEchoStatus::Failed { code, .. } if code == "PERMISSION_DENIED"
        ));
        let msg = s.last_error.as_deref().unwrap_or("");
        assert!(msg.contains("发送失败"), "状态条错误提示, msg={msg}");
        assert!(msg.contains("PERMISSION_DENIED"), "错误码可见, msg={msg}");
    }

    #[test]
    fn blank_or_unowned_session_can_send_ac002_11() {
        // 空白/未归属会话发送：目标 = 当前活动会话，发送路径不依赖归属/
        // workspace 元数据（首个 turn 正常入队，状态以官方投影为准）。
        let mut s = AppState::default();
        // 无 workspace/session meta（未归属）且窗口为空（空白）。
        let cmds = submit_flow(&mut s, "blank-1", "首个 turn");
        assert_eq!(cmds.len(), 1);
        let Cmd::SendPrompt {
            session_id,
            request,
        } = &cmds[0]
        else {
            panic!("预期 SendPrompt，得到 {cmds:?}")
        };
        assert_eq!(session_id, &SessionId("blank-1".into()));
        assert_eq!(request.mode, PromptMode::Queue);
        let w = s.sessions.get("blank-1").unwrap();
        assert_eq!(w.pending().count(), 1, "本地立即回显");
        assert_eq!(w.pending().next().unwrap().text, "首个 turn");
        assert_eq!(w.len(), 0, "空白会话无历史");
    }

    #[test]
    fn prompt_accepted_waits_for_durable_reconciliation_ac002_06() {
        let mut s = AppState::default();
        let cmds = submit_flow(&mut s, "s1", "hi");
        let (session_id, request_id) = match &cmds[0] {
            Cmd::SendPrompt {
                session_id,
                request,
            } => (session_id.clone(), request.request_id.clone()),
            _ => panic!(),
        };
        // accepted 只结束本次命令状态：pending 仍等 durable（follow 是唯一权威）。
        let out = s.handle(AppEvent::PromptAccepted {
            session_id: session_id.clone(),
            request_id: request_id.clone(),
        });
        assert!(out.is_empty());
        assert_eq!(s.sessions.get("s1").unwrap().pending().count(), 1);
        // durable 事件到达 → 对账合并为一条，不再重复显示。
        s.handle(AppEvent::FollowEvent {
            session_id: session_id.clone(),
            event: SessionWireEvent {
                event_type: "user/message".into(),
                seq: Some(SessionSeq(11)),
                time: None,
                request_id: Some(request_id.0.clone()),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: Some(serde_json::json!({"content": "hi"})),
            },
        });
        let w = s.sessions.get("s1").unwrap();
        assert_eq!(w.pending().count(), 0, "durable 对账 retire pending");
        assert_eq!(w.len(), 1, "只保留一个 durable 块");
    }

    // ---------- REQ-002 停止转场与 cancel 竞态（Step 4） ----------

    #[test]
    fn stop_first_press_single_cancel_and_idempotent_ac002_05_10() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true)); // 官方投影 running
        let cmds = s.handle_command(C::StopRunning);
        assert_eq!(
            cmds,
            vec![Cmd::CancelSession(SessionId("s1".into()))],
            "首次 s 只发一次 cancel"
        );
        assert_eq!(s.stop.requested_session, Some(SessionId("s1".into())));
        // 重复 s（同一会话）：幂等，不重复触发、不报错（AC-002-10）。
        assert!(s.handle_command(C::StopRunning).is_empty());
        assert_eq!(s.stop.requested_session, Some(SessionId("s1".into())));
        // 未运行会话：s 无操作。
        let mut s2 = AppState::default();
        s2.handle_command(C::OpenSession(SessionId("s2".into())));
        s2.handle(snapshot("s2", false));
        assert!(s2.handle_command(C::StopRunning).is_empty());
        assert_eq!(s2.stop.requested_session, None);
        // 另一运行中会话不受前一会话的停止转场阻断（per-session 语义）。
        let mut s3 = AppState::default();
        s3.handle_command(C::OpenSession(SessionId("sA".into())));
        s3.handle(snapshot("sA", true));
        s3.handle_command(C::OpenSession(SessionId("sB".into())));
        s3.handle(snapshot("sB", true));
        s3.handle_command(C::OpenSession(SessionId("sA".into())));
        s3.handle_command(C::StopRunning);
        assert_eq!(s3.stop.requested_session, Some(SessionId("sA".into())));
        s3.handle_command(C::OpenSession(SessionId("sB".into())));
        assert_eq!(
            s3.handle_command(C::StopRunning),
            vec![Cmd::CancelSession(SessionId("sB".into()))],
            "B 会话停止不被 A 的转场阻断"
        );
    }

    #[test]
    fn cancel_accepted_keeps_stopping_until_projection_flips() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true));
        s.handle_command(C::StopRunning);
        // accepted：本地「停止中」保持，官方 running 不动（ADR-008）。
        let out = s.handle(AppEvent::CancelAccepted {
            session_id: SessionId("s1".into()),
        });
        assert!(out.is_empty());
        assert_eq!(s.stop.requested_session, Some(SessionId("s1".into())));
        assert!(s.active_running(), "官方投影仍 running");
        // 官方投影翻转 running=false → 转场结束（显示已停止）。
        s.handle(snapshot("s1", false));
        assert_eq!(s.stop.requested_session, None);
        assert!(!s.active_running());
    }

    #[test]
    fn cancel_failed_shows_hint_no_crash_and_retry_works_ac002_09() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true));
        s.handle_command(C::StopRunning);
        let out = s.handle(AppEvent::CancelFailed {
            session_id: SessionId("s1".into()),
            error: ClientError::Remote {
                code: "session/agent-busy".into(),
                message: "忙".into(),
                class: ErrorClass::UserFacing,
            },
        });
        assert!(out.is_empty(), "失败不崩溃、不触发额外命令");
        assert!(
            s.last_error.as_deref().unwrap_or("").contains("停止失败"),
            "状态条 cancel 失败提示"
        );
        assert_eq!(s.stop.requested_session, None, "失败释放转场（可重试）");
        assert!(s.active_running(), "官方投影口径不变（不臆造）");
        // 恢复路径：失败后再次 s 必须能重新发起（不被旧失败状态污染）。
        assert_eq!(
            s.handle_command(C::StopRunning),
            vec![Cmd::CancelSession(SessionId("s1".into()))],
            "失败后可再次手动停止"
        );
        s.handle(snapshot("s1", false));
        assert_eq!(s.stop.requested_session, None);
    }

    #[test]
    fn stale_cancel_results_do_not_disturb_current_transition() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true));
        s.handle_command(C::StopRunning);
        // 陈旧/非在途会话的结果：不 panic、不影响当前转场。
        s.handle(AppEvent::CancelAccepted {
            session_id: SessionId("s-other".into()),
        });
        s.handle(AppEvent::CancelFailed {
            session_id: SessionId("s-other".into()),
            error: ClientError::Transport("eof".into()),
        });
        assert_eq!(s.stop.requested_session, Some(SessionId("s1".into())));
        assert!(s.last_error.is_none(), "非在途失败不覆盖状态条");
        // 在途会话失败后重试仍可用。
        s.handle(AppEvent::CancelFailed {
            session_id: SessionId("s1".into()),
            error: ClientError::Transport("eof".into()),
        });
        assert_eq!(s.stop.requested_session, None);
        assert_eq!(
            s.handle_command(C::StopRunning),
            vec![Cmd::CancelSession(SessionId("s1".into()))]
        );
    }

    #[test]
    fn quit_while_stopping_keeps_first_stop_semantics_ac002_07() {
        // 运行中 + 已请求停止：Ctrl+c 首次仍只确认，二次 cancel→restore→exit。
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true));
        s.handle_command(C::StopRunning);
        let cmds = s.handle_command(C::Quit);
        assert!(cmds.is_empty(), "首次 Ctrl+c 仅确认");
        assert!(s.quit_requested && !s.exited);
        let cmds = s.handle_command(C::Quit);
        assert_eq!(
            cmds,
            vec![
                Cmd::CancelSession(SessionId("s1".into())),
                Cmd::RestoreTerminal,
                Cmd::Exit,
            ]
        );
        assert!(s.exited);
    }

    // ---------- REQ-003：搜索 / 视觉 / 审批 / 大纲 / steer / 草稿（Step 4 Prototype PASS 条件） ----------

    fn assistant_md(s: &mut AppState, sid: &str, seq: u64, md: &str) {
        // 助手内容走 chunk 行到达（REQ-001 窗口模型：assistant 事件先建块，
        // 文本经 FollowChunks 打包）。
        let record = SessionHistoryRecord::Event {
            event: SessionWireEvent {
                event_type: "assistant/message".into(),
                seq: Some(SessionSeq(seq)),
                time: None,
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: Some(serde_json::json!({ "content": md })),
            },
        };
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId(sid.into()),
            cursor: Some(SessionLogOffset(seq)),
            records: vec![record],
            has_more: true,
            projections: Some(serde_json::json!({"running": false})),
        });
        s.handle(AppEvent::FollowChunks {
            session_id: SessionId(sid.into()),
            row: ChunkRow::TextChunks(crate::api::types::ChunkData {
                texts: vec![md.to_string()],
                ..Default::default()
            }),
        });
    }

    #[test]
    fn search_empty_query_never_sends_ac003_19() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        assistant_md(&mut s, "s1", 1, "hello world");
        // 打开搜索：空查询不产生任何搜索命令（AC-003-19）。
        s.handle_command(C::StartSearch);
        assert!(s.search.query.is_empty());
        let cmds = s.handle_command(C::PickerInput(" ".into()));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::DebounceSearch { .. })),
            "空/纯空白查询不发搜索: {cmds:?}"
        );
        // 输入真实词：恰好一个防抖命令。
        let cmds = s.handle_command(C::PickerInput("hello".into()));
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::DebounceSearch { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn search_debounce_keeps_only_latest_generation_ac003_14() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        assistant_md(&mut s, "s1", 1, "alpha beta");
        s.handle_command(C::StartSearch);
        s.handle_command(C::PickerInput("a".into()));
        s.handle_command(C::PickerInput("b".into()));
        // 连续输入：generation 递增，stale 防抖触发被丢弃。
        let stale = s.handle(AppEvent::SearchHistoryDebounced {
            query: "a".into(),
            generation: s.search.history_generation - 1,
        });
        assert!(stale.is_empty(), "stale 防抖触发丢弃");
        // 最新 generation 才发出 session/search。
        let fresh = s.handle(AppEvent::SearchHistoryDebounced {
            query: "ab".into(),
            generation: s.search.history_generation,
        });
        assert_eq!(
            fresh
                .iter()
                .filter(|c| matches!(c, Cmd::SearchSessions { .. }))
                .count(),
            1,
            "最新 generation 恰好一次搜索"
        );
        assert!(s.search.history_loading);
        // 快速改词：旧结果到达时 generation 已变 → 丢弃，不覆盖。
        s.handle_command(C::PickerInput("c".into()));
        s.handle(AppEvent::SearchResult {
            generation: s.search.history_generation - 1,
            items: vec![],
            has_more: false,
        });
        assert!(s.search.history_loading, "stale 结果不解除 loading");
        // 最新结果正常落地。
        s.handle(AppEvent::SearchResult {
            generation: s.search.history_generation,
            items: vec![],
            has_more: false,
        });
        assert!(!s.search.history_loading);
    }

    #[test]
    fn context_yank_code_block_copies_whole_block_ac003_03_12() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        assistant_md(&mut s, "s1", 1, "```rust\nfn main() {}\n```\n\n其它文本");
        let cmds = s.handle_command(C::YankContext);
        assert_eq!(cmds.len(), 1);
        let Cmd::CopyToClipboard { text } = &cmds[0] else {
            panic!("预期 CopyToClipboard，得到 {cmds:?}")
        };
        assert_eq!(text, "fn main() {}\n", "代码块整块复制");
        assert_eq!(s.yank.last.as_deref(), Some("fn main() {}\n"));
    }

    #[test]
    fn visual_selection_v_y_copies_blocks_ac003_12() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(3)),
            records: (1..=3)
                .map(|n| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "user/message".into(),
                        seq: Some(SessionSeq(n)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: Some(serde_json::json!({"content": format!("行{n}")})),
                    },
                })
                .collect(),
            has_more: false,
            projections: Some(serde_json::json!({"running": false})),
        });
        // v 进入 VISUAL（字符模式，锚=焦点块）。
        s.handle_command(C::VisualStart { line: false });
        assert_eq!(s.mode, Mode::Visual);
        // j 扩展（行模式才跨块；字符模式单块）。
        s.handle_command(C::MoveDown);
        let cmds = s.handle_command(C::YankContext);
        assert_eq!(s.mode, Mode::Normal, "复制后退出 VISUAL");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::CopyToClipboard { .. }));
        assert!(s.yank.last.is_some());
        // V 行模式跨块复制。
        s.handle_command(C::VisualStart { line: true });
        s.handle_command(C::MoveDown);
        let cmds = s.handle_command(C::YankContext);
        let Cmd::CopyToClipboard { text } = &cmds[0] else {
            panic!()
        };
        assert!(text.contains('\n'), "行模式跨块: {text}");
    }

    #[test]
    fn approval_arrives_forces_mode_replies_once_and_restores_ac003_07_17() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        let ev = ApprovalEvent {
            client_id: "c-1".into(),
            event_id: "e-1".into(),
            raw: serde_json::json!({"type": "approval/request", "reason": "x"}),
        };
        // 审批到达：强制 APPROVAL。
        assert!(s
            .handle(AppEvent::ApprovalRequest { event: ev.clone() })
            .is_empty());
        assert_eq!(s.mode, Mode::Approval);
        assert_eq!(s.approval.prev_mode, Mode::Normal);
        // 重复投递同 event_id：幂等忽略（模式 15 去重）。
        s.handle(AppEvent::ApprovalRequest { event: ev.clone() });
        assert!(s.approval.visible);
        // y → 恰好一次 ReplyApproval；回复中再按 n 不产生第二次。
        let cmds = s.handle_command(C::ApprovalAllow);
        assert_eq!(cmds.len(), 1);
        assert!(
            matches!(&cmds[0], Cmd::ReplyApproval { outcome, .. } if *outcome == ApprovalOutcome::AllowedOnce)
        );
        assert!(s.approval.reply_inflight);
        assert!(
            s.handle_command(C::ApprovalReject).is_empty(),
            "in-flight 幂等"
        );
        // 回复成功：回先前模式，运行状态不丢。
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        assert_eq!(s.mode, Mode::Normal);
        assert!(!s.approval.visible);
        assert_eq!(s.approval.last_outcome, Some(ApprovalOutcome::AllowedOnce));
        // 再次事件（新 event_id）可再次决策（恢复路径）。
        let ev2 = ApprovalEvent {
            client_id: "c-1".into(),
            event_id: "e-2".into(),
            raw: serde_json::json!({"type": "approval/request"}),
        };
        s.handle(AppEvent::ApprovalRequest { event: ev2 });
        assert_eq!(s.mode, Mode::Approval);
        assert_eq!(
            s.handle_command(C::ApprovalReject).len(),
            1,
            "新事件可再次回复"
        );
    }

    #[test]
    fn approval_q_esc_are_cancel_not_quit_ac003_07() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true));
        s.handle(AppEvent::ApprovalRequest {
            event: ApprovalEvent {
                client_id: "c".into(),
                event_id: "e".into(),
                raw: serde_json::json!({"type": "approval/request"}),
            },
        });
        // q 在 APPROVAL = cancelled 决策（不退出）。
        let cmds = s.handle_command(C::Quit);
        assert_eq!(cmds.len(), 1);
        assert!(
            matches!(&cmds[0], Cmd::ReplyApproval { outcome, .. } if *outcome == ApprovalOutcome::Cancelled)
        );
        assert!(!s.exited, "q 不退出程序");
    }

    #[test]
    fn approval_reply_failure_fails_closed_with_waiting_hint_ac003_17_18() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle(AppEvent::ApprovalRequest {
            event: ApprovalEvent {
                client_id: "c".into(),
                event_id: "e".into(),
                raw: serde_json::json!({"type": "approval/request"}),
            },
        });
        s.handle_command(C::ApprovalAllow);
        // 回复失败：不授权（fail closed），弹窗收缩为等待审批，回到先前模式。
        s.handle(AppEvent::ApprovalReplyFailed {
            outcome: ApprovalOutcome::AllowedOnce,
            error: ClientError::Transport("eof".into()),
        });
        assert!(s.approval.waiting_hint);
        assert!(!s.approval.visible);
        assert_eq!(s.mode, Mode::Normal);
        assert!(
            s.last_error.as_deref().unwrap_or("").contains("官方 web"),
            "指引官方 web 完成"
        );
        // 恢复路径：下一个新事件仍可正常决策（不被旧失败污染）。
        s.handle(AppEvent::ApprovalRequest {
            event: ApprovalEvent {
                client_id: "c".into(),
                event_id: "e2".into(),
                raw: serde_json::json!({"type": "approval/request"}),
            },
        });
        assert_eq!(s.mode, Mode::Approval);
        assert!(!s.approval.waiting_hint);
        assert_eq!(s.handle_command(C::ApprovalAllow).len(), 1);
    }

    #[test]
    fn steer_mode_reads_official_running_projection_ac003_06() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true)); // 官方投影 running
        s.handle_command(C::InsertMode);
        assert!(s.composer.steer, "运行中 i → steer");
        // 发送使用 mode:"steer"。
        s.handle_command(C::PickerInput("追加".into()));
        let cmds = s.handle_command(C::SubmitInput);
        let Cmd::SendPrompt { request, .. } = &cmds[0] else {
            panic!()
        };
        assert_eq!(request.mode, PromptMode::Steer);
        // 未运行 → queue（REQ-002 原义）。
        let mut s2 = AppState::default();
        s2.handle_command(C::OpenSession(SessionId("s1".into())));
        s2.handle(snapshot("s1", false));
        s2.handle_command(C::InsertMode);
        assert!(!s2.composer.steer);
        s2.handle_command(C::PickerInput("排队".into()));
        let cmds = s2.handle_command(C::SubmitInput);
        let Cmd::SendPrompt { request, .. } = &cmds[0] else {
            panic!()
        };
        assert_eq!(request.mode, PromptMode::Queue);
    }

    #[test]
    fn steer_unavailable_keeps_draft_and_hints_ac003_16() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", true));
        s.handle_command(C::InsertMode);
        s.handle_command(C::PickerInput("steer-me".into()));
        let cmds = s.handle_command(C::SubmitInput);
        let (sid, rid) = match &cmds[0] {
            Cmd::SendPrompt {
                session_id,
                request,
            } => (session_id.clone(), request.request_id.clone()),
            _ => panic!(),
        };
        s.handle(AppEvent::PromptFailed {
            session_id: sid.clone(),
            request_id: rid,
            error: ClientError::Remote {
                code: "session/steer-unavailable".into(),
                message: "轮次已结束".into(),
                class: ErrorClass::UserFacing,
            },
        });
        assert!(
            s.last_error
                .as_deref()
                .unwrap_or("")
                .contains("steer 不可用"),
            "状态条错误"
        );
        // 草稿保留（注册表），可再次 i 编辑或改排队发送（恢复路径）。
        assert_eq!(
            s.drafts.get(&sid).map(|d| d.text.as_str()),
            Some("steer-me")
        );
        s.handle_command(C::InsertMode);
        assert_eq!(s.draft.as_ref().map(|d| d.text.as_str()), Some("steer-me"));
    }

    #[test]
    fn draft_survives_session_switch_and_returns_ac003_11() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle_command(C::InsertMode);
        s.handle_command(C::PickerInput("s1 草稿".into()));
        s.handle_command(C::ClosePicker); // Esc 收起
                                          // 切到 s2 再切回 s1：草稿随 bound_session 保留（D-20）。
        s.handle_command(C::OpenSession(SessionId("s2".into())));
        s.handle(snapshot("s2", false));
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle_command(C::InsertMode);
        assert_eq!(
            s.draft.as_ref().map(|d| d.text.as_str()),
            Some("s1 草稿"),
            "跨会话草稿恢复（仅内存）"
        );
        assert_eq!(
            s.draft.as_ref().map(|d| d.bound_session.0.as_str()),
            Some("s1")
        );
    }

    #[test]
    fn input_history_up_down_ac003_10() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        // 发送两条进入历史。
        for text in ["第一条", "第二条"] {
            s.handle_command(C::InsertMode);
            s.handle_command(C::PickerInput(text.into()));
            s.handle_command(C::SubmitInput);
        }
        s.handle_command(C::InsertMode);
        s.handle_command(C::PickerInput("正在编辑".into()));
        s.handle_command(C::HistoryPrev);
        assert_eq!(s.draft.as_ref().map(|d| d.text.as_str()), Some("第二条"));
        s.handle_command(C::HistoryPrev);
        assert_eq!(s.draft.as_ref().map(|d| d.text.as_str()), Some("第一条"));
        s.handle_command(C::HistoryNext);
        assert_eq!(s.draft.as_ref().map(|d| d.text.as_str()), Some("第二条"));
        s.handle_command(C::HistoryNext);
        assert_eq!(
            s.draft.as_ref().map(|d| d.text.as_str()),
            Some("正在编辑"),
            "越过最新恢复原草稿"
        );
    }

    #[test]
    fn open_external_at_cursor_only_for_links_ac003_04_20() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        assistant_md(&mut s, "s1", 1, "见 [文档](https://example.com/x) 部署");
        s.focus = Focus::Center;
        let cmds = s.handle_command(C::OpenSelected);
        assert_eq!(
            cmds,
            vec![Cmd::OpenExternal {
                target: "https://example.com/x".into()
            }]
        );
        // 光标块不含链接 → no-op 提示（不误触发系统打开）。
        s.handle_command(C::OpenSession(SessionId("s2".into())));
        assistant_md(&mut s, "s2", 1, "没有链接的段落");
        s.focus = Focus::Center;
        let cmds = s.handle_command(C::OpenSelected);
        assert!(cmds.is_empty());
        assert!(
            s.last_error.as_deref().unwrap_or("").contains("没有链接"),
            "状态条 no-op 提示"
        );
    }

    #[test]
    fn search_confirm_history_hit_opens_session_ac003_03() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        assistant_md(&mut s, "s1", 1, "hello");
        s.handle_command(C::StartSearch);
        // 全历史命中另一会话 → Enter 打开该会话（D-17 收缩）。
        s.search.history_hits = vec![SearchHit {
            session_id: SessionId("sess-9".into()),
            snippet: "deploy 排查 …".into(),
        }];
        let cmds = s.handle_command(C::PickerConfirm);
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::OpenFollow { .. }))
                .count(),
            1,
            "打开命中会话"
        );
        assert_eq!(s.active_session.as_ref(), Some(&SessionId("sess-9".into())));
        // snippet 二次窗口内定位：搜索 overlay 保持打开且 query = snippet
        // （截断省略号已去掉）。
        assert!(s.search.open, "搜索 overlay 保持打开（二次定位）");
        assert_eq!(s.search.query, "deploy 排查");
        // 窗口快照到达（含命中内容）→ 窗口内即时命中自动重算并高亮。
        assistant_md(&mut s, "sess-9", 10, "deploy 排查 的结论在这里");
        assert!(
            !s.search.window_matches.is_empty(),
            "snippet 在窗口内二次定位命中"
        );
        assert_eq!(s.search.cursor, 0);
        // Esc 关闭搜索，会话保持打开。
        s.handle_command(C::ClosePicker);
        assert!(!s.search.open);
        assert_eq!(s.active_session.as_ref(), Some(&SessionId("sess-9".into())));
    }

    #[test]
    fn outline_jump_turn_uses_load_through_when_not_loaded_ac003_09() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(50)),
            records: (41..=50)
                .map(|n| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "assistant/message".into(),
                        seq: Some(SessionSeq(n)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: Some(serde_json::json!({"content": format!("轮{n}")})),
                    },
                })
                .collect(),
            has_more: true,
            projections: Some(serde_json::json!({
                "running": false,
                "turnOutline": [
                    {"turn": 1, "seq": 5, "prompt": "早轮"},
                    {"turn": 2, "seq": 45, "prompt": "近轮"}
                ]
            })),
        });
        // 窗口已含 seq 45 → ] 跳转直接落位。
        let cmds = s.handle_command(C::NextTurn);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::LoadThrough { .. })),
            "已加载轮直接落位: {cmds:?}"
        );
        assert_eq!(
            s.cursor_block,
            s.active_window()
                .and_then(|w| w.offset_of(SessionSeq(45)))
                .unwrap()
        );
        // 目标 seq 5 未加载 → loadThrough 分页。
        let cmds = s.handle_command(C::PrevTurn);
        assert_eq!(cmds, vec![Cmd::LoadThrough { seq: SessionSeq(5) }]);
        // LoadThroughPage 合并后（seq 去重，REQ-001）窗口无重复无空洞。
        let before = s.active_window().unwrap().len();
        s.handle(AppEvent::LoadThroughPage {
            session_id: SessionId("s1".into()),
            records: (1..=40)
                .map(|n| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "assistant/message".into(),
                        seq: Some(SessionSeq(n)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: Some(serde_json::json!({"content": format!("轮{n}")})),
                    },
                })
                .collect(),
            has_more: Some(false),
        });
        let after = s.active_window().unwrap().len();
        assert_eq!(after, before + 40, "seq 去重合并无重复: {before}→{after}");
        // 落位：目标 seq 5 已在窗口内 → 游标落位、loadThrough 结束。
        assert_eq!(
            s.cursor_block,
            s.active_window()
                .and_then(|w| w.offset_of(SessionSeq(5)))
                .unwrap(),
            "loadThrough 完成后按 seq 落位（AC-003-09）"
        );
        assert!(s.load_through_target.is_none());
    }

    #[test]
    fn load_through_requeues_until_target_covered_ac003_09() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(100)),
            records: (81..=100)
                .map(|n| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "user/message".into(),
                        seq: Some(SessionSeq(n)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: Some(serde_json::json!({"content": format!("轮{n}")})),
                    },
                })
                .collect(),
            has_more: true,
            projections: Some(serde_json::json!({
                "turnOutline": [{"turn": 1, "seq": 10, "prompt": "很早就轮"}]
            })),
        });
        let cmds = s.handle_command(C::NextTurn);
        assert_eq!(
            cmds,
            vec![Cmd::LoadThrough {
                seq: SessionSeq(10)
            }]
        );
        // 第一页（seq 41..=80）未覆盖目标 10 且有更多历史 → reducer 续页。
        let cmds = s.handle(AppEvent::LoadThroughPage {
            session_id: SessionId("s1".into()),
            records: (41..=80)
                .map(|n| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "user/message".into(),
                        seq: Some(SessionSeq(n)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: Some(serde_json::json!({"content": format!("轮{n}")})),
                    },
                })
                .collect(),
            has_more: Some(true),
        });
        assert_eq!(
            cmds,
            vec![Cmd::LoadThrough {
                seq: SessionSeq(10)
            }],
            "未覆盖且有更多历史 → 续页（逐页命令，不阻塞渲染）"
        );
        assert_eq!(s.load_through_pages, 1);
        // 第二页覆盖目标 → 落位并结束。
        let cmds = s.handle(AppEvent::LoadThroughPage {
            session_id: SessionId("s1".into()),
            records: (1..=40)
                .map(|n| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "user/message".into(),
                        seq: Some(SessionSeq(n)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: Some(serde_json::json!({"content": format!("轮{n}")})),
                    },
                })
                .collect(),
            has_more: Some(false),
        });
        assert!(cmds.is_empty(), "覆盖后结束: {cmds:?}");
        assert_eq!(
            s.cursor_block,
            s.active_window()
                .and_then(|w| w.offset_of(SessionSeq(10)))
                .unwrap()
        );
        assert!(s.load_through_target.is_none());
    }

    #[test]
    fn approval_pending_projection_shows_waiting_hint_ac003_18() {
        // 目标版本不转发 approval/request：官方投影出现待审批信号 → 状态条
        // 等待审批（弹窗不出现、不阻塞）。
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(0)),
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({"running": true, "awaitingApproval": true})),
        });
        assert!(s.approval.waiting_hint, "投影待审批信号 → 状态条等待审批");
        assert!(!s.approval.visible);
        // 投影清除 → hint 消失；弹窗事件路径不受影响。
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(0)),
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({"running": false, "awaitingApproval": false})),
        });
        assert!(!s.approval.waiting_hint);
        // 弹窗事件到达 → 正常审批模态（hint 关闭）。
        s.handle(AppEvent::ApprovalRequest {
            event: ApprovalEvent {
                client_id: "c".into(),
                event_id: "e".into(),
                raw: serde_json::json!({"type": "approval/request"}),
            },
        });
        assert!(s.approval.visible);
        assert!(!s.approval.waiting_hint);
        // 判定函数形状容忍：字符串/数组也识别；缺失/false 不识别。
        assert!(projections_await_approval(
            &serde_json::json!({"pendingApproval": "awaiting"})
        ));
        assert!(projections_await_approval(
            &serde_json::json!({"approvalPending": [1]})
        ));
        assert!(!projections_await_approval(
            &serde_json::json!({"running": true})
        ));
    }

    #[test]
    fn search_permission_error_does_not_retry_window_search_unaffected_ac003_13() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        assistant_md(&mut s, "s1", 1, "deploy the operator");
        s.handle_command(C::StartSearch);
        s.handle_command(C::PickerInput("deploy".into()));
        let gen = s.search.history_generation;
        s.handle(AppEvent::SearchError {
            generation: gen,
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "无权限".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(
            s.search
                .history_error
                .as_deref()
                .unwrap_or("")
                .contains("权限"),
            "权限错误提示"
        );
        assert!(!s.is_reconnecting(), "权限错误不触发重连");
        // 窗口内即时搜索离线可用。
        assert!(!s.search.window_matches.is_empty(), "窗口命中不受影响");
        // 恢复路径：改词后可再次发起全历史搜索。
        s.handle_command(C::PickerInput("x".into()));
        let cmds = s.handle_command(C::PickerInput("y".into()));
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::DebounceSearch { .. }))
                .count(),
            1,
            "恢复后可再次搜索"
        );
    }

    #[test]
    fn copy_done_feedback_and_unavailable_fallback_ac003_08() {
        let mut s = AppState::default();
        s.handle(AppEvent::CopyDone {
            backend: YankBackend::System,
            ok: true,
        });
        assert_eq!(s.yank.backend, YankBackend::System);
        assert_eq!(s.yank.toast.as_deref(), Some("copied"));
        // 降级失败 → Unavailable + 失败提示（不崩溃）。
        s.handle(AppEvent::CopyDone {
            backend: YankBackend::Osc52,
            ok: false,
        });
        assert_eq!(s.yank.backend, YankBackend::Unavailable);
        assert!(s.yank.toast.is_none());
        assert!(
            s.last_error
                .as_deref()
                .unwrap_or("")
                .contains("剪贴板不可用"),
            "失败提示"
        );
    }

    #[test]
    fn search_n_and_shift_n_cycle_matches_ac003_05() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(3)),
            records: (1..=3)
                .map(|n| SessionHistoryRecord::Event {
                    event: SessionWireEvent {
                        event_type: "user/message".into(),
                        seq: Some(SessionSeq(n)),
                        time: None,
                        request_id: None,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: None,
                        data: Some(serde_json::json!({"content": format!("deploy {n}")})),
                    },
                })
                .collect(),
            has_more: false,
            projections: Some(serde_json::json!({"running": false})),
        });
        s.handle_command(C::StartSearch);
        s.handle_command(C::PickerInput("deploy".into()));
        assert_eq!(s.search.window_matches.len(), 3, "三个窗口命中");
        assert_eq!(s.search.cursor, 0);
        // 编辑段：n/N 是输入字符（查询可含这些字母，AC-003-14 无吞字）。
        assert!(!s.search.results_locked);
        s.handle_command(C::SearchNext);
        assert_eq!(s.search.query, "deployn", "编辑段 n 为输入字符");
        s.handle_command(C::PickerBackspace);
        assert_eq!(s.search.query, "deploy");
        // Enter → 结果巡览段（锁定）：n/N 才是巡览命令（AC-003-05）。
        s.handle_command(C::PickerConfirm);
        assert!(s.search.results_locked);
        s.handle_command(C::SearchNext);
        assert_eq!(s.search.cursor, 1, "n 前进");
        s.handle_command(C::SearchPrev);
        assert_eq!(s.search.cursor, 0, "N 后退");
        s.handle_command(C::SearchPrev);
        assert_eq!(s.search.cursor, 2, "N 环绕到末尾");
        s.handle_command(C::SearchNext);
        assert_eq!(s.search.cursor, 0, "n 环绕回开头");
        // 当前匹配高亮锚定：焦点块游标跳到命中块（窗口块下标，0-based）。
        let expected_idx = s
            .active_window()
            .and_then(|w| w.offset_of(s.search_index.items()[s.search.window_matches[0]].seq))
            .expect("命中块在窗口中");
        assert_eq!(s.cursor_block, expected_idx, "游标落位命中块");
    }

    #[test]
    fn search_yank_copies_current_match_text_ac003_03() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        assistant_md(&mut s, "s1", 1, "```json\n{\"k\": \"v\"}\n```\n\n说明");
        s.handle_command(C::StartSearch);
        s.handle_command(C::PickerInput("k".into()));
        // Enter 锁定结果巡览段（当前命中是 code 块：索引文本 = 代码内容）。
        s.handle_command(C::PickerConfirm);
        assert!(s.search.results_locked);
        let cmds = s.handle_command(C::YankContext);
        assert_eq!(cmds.len(), 1);
        let Cmd::CopyToClipboard { text } = &cmds[0] else {
            panic!("预期复制, 得到 {cmds:?}")
        };
        assert_eq!(text, "{\"k\": \"v\"}\n", "SEARCH y 复制整块代码");
    }
}
