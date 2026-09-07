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

// REQ-009：`dshtui monitor` 独立状态机与事件循环（同一二进制共存）。
pub mod monitor;

use std::collections::{HashMap, HashSet};

use crate::api::types::ChunkRow;
use crate::api::types::{
    ApprovalEvent, ApprovalOutcome, AttachmentId, ControlItem, ListItemRaw, PromptContentPart,
    PromptMode, PromptRequest, SearchHit, SessionHistoryRecord, SessionId, SessionLogOffset,
    SessionRequestId, SessionSeq, SessionWireEvent,
};
use crate::api::{ClientError, ErrorClass};
use crate::model::{
    block_plain_text, block_yank_target, detail_for, selection_text, ApplyEffect, AttachmentRef,
    DraftRegistry, DraftState, FoldState, ImageViewState, Incoming, InputHistory,
    ProjectionSnapshot, SearchIndex, SearchKindFilter, SessionStore, TrajIncoming, VisualMode,
    VisualSelection, WorkspaceStore, YankBackend, YankState,
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
    /// Trajectory（REQ-005 V0.3，D-25）：顶部 Tab 独立模式；右栏详情为
    /// Trajectory 内焦点子层（`focus==Details` + `traj.detail_open`）。
    Trajectory,
    /// 模型目录 overlay（REQ-006 FR-006-01，`M` 打开；ADDR-007 独立模态，
    /// 不串 SEARCH）。
    ModelCatalog,
    /// 命令面板 overlay（REQ-006 FR-006-03，`:` 打开；ADR-007 独立模态，
    /// 不串 SEARCH）。
    CommandPalette,
    /// @ 提及候选（REQ-007 AC-007-23；composer INSERT 内 `@` 触发）。
    Mention,
    /// subagent 目录（REQ-007 FR-007-01；`:subagents` 打开）。
    Subagent,
    /// goal 面板（REQ-007 FR-007-02 half；`:goal` 打开）。
    Goal,
    /// jobs 只读面板（REQ-007 FR-007-02 half；`:jobs` 打开）。
    Jobs,
    /// settings 面板（REQ-007 FR-007-03 half；`:settings` 打开）。
    Settings,
    /// skills 目录（REQ-007 FR-007-03 half；`:skills` 打开）。
    Skills,
    /// 会话导出（REQ-007 FR-007-04；`:export` 打开）。
    Export,
    /// 消息动作菜单（REQ-007 AC-007-27/28；`m` 打开）。
    MessageAction,
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

/// Sidebar 行光标（REQ-006 FR-006-02）：Focus::Sidebar 下 j/k 移动、
/// Enter 打开光标行。与 active_session 高亮分离——光标标记导航行，
/// session 行的 `●`/`○` 仍标运行/空闲（Prototype PASS：不串语义）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SidebarState {
    /// 渲染行下标（`sidebar_rows` 视图态行序，UI 与 reducer 共享 seam）。
    pub cursor: usize,
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
    /// REQ-007 AC-007-31：query 历史回忆游标（None=不在回忆；
    /// Some(i)=正在回看第 i 条 recent）。仅空 query 编辑态可用 ↑ 触发。
    pub recall_cursor: Option<usize>,
}

impl SearchState {
    /// 前缀过滤 + 有效查询词（`/c x` → (Code, "x")）。
    pub fn terms(&self) -> (SearchKindFilter, String) {
        let (filter, term) = SearchKindFilter::from_prefix(&self.query);
        (filter, term.to_string())
    }
}

/// APPROVAL 模态状态（REQ-003 §5 `ApprovalState`；仅内存）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalState {
    pub visible: bool,
    /// 当前展示项 = 队列 active 的镜像（REQ-006 D-036 additive：事件恒为
    /// queue active 镜像，队列在途覆盖隐患由入队语义消除）。
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
    // ---------- REQ-006 审批队列扩展（D-036 additive） ----------
    /// 串行审批队列（纯模型；pending/active/failed + 有界 granted 去重）。
    pub queue: crate::model::ApprovalQueue,
    /// danger-full-access 当前项第二层风险确认（AC-006-16）。
    pub acked: bool,
    /// 批量 allowed-once 模式：队列自动续发（≤1 在途，AC-006-15）；danger
    /// 项停点等 ack 后继续。
    pub batch_allow: bool,
    /// 审批列表视图（`L` 打开：j/k 移动、`r` 重试失败项、`A` 批量、q/Esc
    /// 回单条槽）。
    pub list_open: bool,
    /// 列表光标（`ApprovalQueue::list()` 下标）。
    pub list_cursor: usize,
    /// 会话 approval/policy 只读展示（ask|never；AC-006-17/D-037，从官方
    /// 投影宽容解析，TUI 无切换入口）。
    pub policy_display: Option<&'static str>,
}

impl Default for ApprovalState {
    fn default() -> Self {
        Self {
            visible: false,
            event: None,
            last_outcome: None,
            reply_inflight: false,
            prev_mode: Mode::Normal,
            waiting_hint: false,
            toast: None,
            queue: crate::model::ApprovalQueue::new(),
            acked: false,
            batch_allow: false,
            list_open: false,
            list_cursor: 0,
            policy_display: None,
        }
    }
}

/// 模型目录 overlay 阶段（REQ-006 FR-006-01；M 打开后异步拉取）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatalogPhase {
    /// `session/modelCatalog` 拉取中（加载失败/断网可观察，AC-006-06）。
    #[default]
    Loading,
    /// 目录就绪（含空目录 → 空态提示，AC-006-07 由 query 命中判定）。
    Ready,
    /// 加载失败（保留错误提示，可重开/重试）。
    Error,
}

/// Model catalog overlay 状态（REQ-006 §5 `ModelCatalogState`；仅内存）。
///
/// - `index` 为扁平化后的全量目录（本地 nucleo 过滤，AC-006-01/07 即时性=
///   本地，官方 modelCatalog 零参数）。
/// - `current_model`/`next_model` 是官方 projections.modelSelection 的只读
///   镜像（ADR-008，reducer 在快照/切换时刷新，不自算）。
/// - effort 子阶段（`effort: Some`）：选中模型自带 reasoning.efforts 子集
///   时，Enter 先选 effort 再提交（不越界 V0.4）。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCatalogState {
    pub visible: bool,
    pub phase: CatalogPhase,
    /// 扁平目录（含描述/efforts 元数据；查询命中实时过滤）。
    pub index: crate::model::CatalogIndex,
    /// 输入行 query（nucleo 本地即时过滤）。
    pub query: String,
    /// 过滤后命中列表选中下标（j/k 移动）。
    pub selection: usize,
    /// 官方投影 current/next 的展示串（reducer 刷新，ADR-008）。
    pub current_model: Option<String>,
    pub next_model: Option<String>,
    /// 最近一次加载失败提示（AC-006-06）。
    pub load_error: Option<String>,
    /// 最近一次 selectModel 失败 `error.code`（AC-006-09 状态条提示）。
    pub last_error_code: Option<String>,
    /// selectModel 在途（provider/model）：防重入（同一弹窗只提交一次）。
    pub selecting: Option<(String, String)>,
    /// effort 子阶段（选中模型带 reasoning.efforts 时进入）。
    pub effort: Option<EffortPick>,
}

impl Default for ModelCatalogState {
    fn default() -> Self {
        Self {
            visible: false,
            phase: CatalogPhase::Loading,
            index: crate::model::CatalogIndex::new(),
            query: String::new(),
            selection: 0,
            current_model: None,
            next_model: None,
            load_error: None,
            last_error_code: None,
            selecting: None,
            effort: None,
        }
    }
}

impl ModelCatalogState {
    /// 重置为「打开即拉取」初态（清查询/选中/错误，保留目录在重开时刷新）。
    pub fn reset_for_open(&mut self) {
        self.visible = true;
        self.phase = CatalogPhase::Loading;
        self.query.clear();
        self.selection = 0;
        self.load_error = None;
        self.last_error_code = None;
        self.selecting = None;
        self.effort = None;
    }
}

/// effort 子阶段：选中模型的 reasoning effort 候选（id 列表）+ 光标。
#[derive(Debug, Clone, PartialEq)]
pub struct EffortPick {
    pub provider_id: String,
    pub model_id: String,
    pub model_name: String,
    /// 候选 effort id（wire `efforts[].id`；off/low/high/max…）。
    pub efforts: Vec<String>,
    pub default: Option<String>,
    pub cursor: usize,
}

/// 命令面板候选的动作类型（REQ-006 FR-006-03 / D-035 + FR-006-02 操作半）。
/// workspace/session 操作项进入 palette 输入/确认子阶段（Step 6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteAction {
    /// ≡ `M` 打开模型目录。
    ModelCatalog,
    /// ≡ `gv` 循环侧栏视图（本地态）。
    CycleSidebarView,
    /// ≡ `h` 折叠全部 workspace。
    CollapseAll,
    /// ≡ `l` 展开全部 workspace。
    ExpandAll,
    /// 新建会话（`session/create`，空 workspace；成功刷新列表）。
    NewSession,
    /// `help` 打开帮助。
    Help,
    /// 会话操作（目标 = active_session）。
    ForkSession,
    RenameSession,
    ArchiveSession,
    /// workspace 操作（目标 = sidebar 光标行 workspace；无则提示）。
    NewWorkspace,
    RenameWorkspace,
    DeleteWorkspace,
    /// 移动会话到目标 workspace（输入 workspace id / 空 = 未分组）。
    MoveSession,
    /// REQ-007：`:` theme 切换（dark↔light，立即重绘 + save_theme 持久化，
    /// AC-007-20）。
    ToggleTheme,
    /// REQ-007：`:` `edit` —— 用 $EDITOR 编辑当前 composer 草稿（AC-007-25）。
    EditWithEditor,
    /// REQ-007：`:subagents` 打开子代理目录（FR-007-01）。
    OpenSubagents,
    /// REQ-007：`:goal` 打开 goal 面板（FR-007-02）。
    OpenGoal,
    /// REQ-007：`:jobs` 打开 jobs 只读面板。
    OpenJobs,
    /// REQ-007：`:settings` 打开 settings 面板。
    OpenSettings,
    /// REQ-007：`:skills` 打开 skills 目录。
    OpenSkills,
    /// REQ-007：`:export` 打开会话导出。
    OpenExport,
}

/// 会话/workspace 写操作（REQ-006 FR-006-02 操作半；wire 端点 Step 1 已封装）。
#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceOperation {
    /// `session/fork`（atSeq=None：当前头部 fork）。
    ForkSession { session_id: SessionId },
    /// `session/rename`。
    RenameSession {
        session_id: SessionId,
        title: String,
    },
    /// `workspace/archiveSession`（归档=删除当前会话，web 按钮语义）。
    ArchiveSession { session_id: SessionId },
    /// `workspace/create`（path）。
    NewWorkspace { path: String },
    /// `workspace/rename`。
    RenameWorkspace {
        workspace_id: crate::api::types::WorkspaceId,
        title: String,
    },
    /// `workspace/delete`（危险）。
    DeleteWorkspace {
        workspace_id: crate::api::types::WorkspaceId,
    },
    /// `workspace/insert_session_before`（移动当前会话到目标 workspace 末尾；
    /// 目标 workspace 必填——wire 无「移出分组」端点）。
    MoveSession {
        session_id: SessionId,
        target_workspace: crate::api::types::WorkspaceId,
    },
}

impl WorkspaceOperation {
    /// 操作名（通知/错误文案与单飞去重键）。
    pub fn label(&self) -> &'static str {
        match self {
            WorkspaceOperation::ForkSession { .. } => "fork session",
            WorkspaceOperation::RenameSession { .. } => "rename session",
            WorkspaceOperation::ArchiveSession { .. } => "archive session",
            WorkspaceOperation::NewWorkspace { .. } => "new workspace",
            WorkspaceOperation::RenameWorkspace { .. } => "rename workspace",
            WorkspaceOperation::DeleteWorkspace { .. } => "delete workspace",
            WorkspaceOperation::MoveSession { .. } => "move session",
        }
    }

    /// 是否为破坏性操作（archive/delete 需二次确认）。
    pub fn dangerous(&self) -> bool {
        matches!(
            self,
            WorkspaceOperation::ArchiveSession { .. } | WorkspaceOperation::DeleteWorkspace { .. }
        )
    }
}

/// 消息动作菜单目标（AC-007-27）：聚焦块的捕获快照。
#[derive(Debug, Clone, PartialEq)]
pub struct MessageActionTarget {
    pub session_id: SessionId,
    pub seq: u64,
    /// block 种类：UserMessage（retry 需要 content）| AssistantMessage
    /// （feedback 需要 message_id）。
    pub kind: MsgTargetKind,
    /// retry 重发内容（UserMessage content）。
    pub user_text: Option<String>,
    /// assistant message.id（feedback 定位锚）。
    pub message_id: Option<String>,
    /// 目标所属 turn 是否 running（运行中动作需二次确认，AC-007-28）。
    pub running: bool,
    /// 是否为静止轮次的末条 user 消息（branch 门槛）。
    pub is_last_user: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgTargetKind {
    User,
    Assistant,
}

/// goal 写操作（REQ-007 FR-007-02；CAS revision 由面板 inflight 携带，
/// wire 端点 agentId=active_session）。
#[derive(Debug, Clone, PartialEq)]
pub enum GoalMutation {
    /// `goals/create`（objective + 可选 maxGoalRounds）。
    Create {
        objective: String,
        max_goal_rounds: Option<u64>,
    },
    /// `goals/edit`（objective）。
    Edit {
        objective: String,
    },
    /// `goals/pause` / `resume` / `complete`。
    Pause,
    Resume,
    Complete,
    /// `goals/clear`（危险，二次确认）。
    Clear,
}

impl GoalMutation {
    pub fn op_kind(&self) -> crate::model::GoalOpKind {
        match self {
            GoalMutation::Create { .. } => crate::model::GoalOpKind::Create,
            GoalMutation::Edit { .. } => crate::model::GoalOpKind::Edit,
            GoalMutation::Pause => crate::model::GoalOpKind::Pause,
            GoalMutation::Resume => crate::model::GoalOpKind::Resume,
            GoalMutation::Complete => crate::model::GoalOpKind::Complete,
            GoalMutation::Clear => crate::model::GoalOpKind::Clear,
        }
    }
}

/// palette 操作子阶段（Step 6）：参数输入 / 破坏性二次确认。
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteStage {
    /// 文本参数输入（重命名标题/新建 workspace path/移动目标）。
    Input {
        prompt: &'static str,
        kind: OpArgKind,
    },
    /// 破坏性确认（`y` 执行 / 其它键取消）。
    ConfirmDanger {
        label: String,
        op: WorkspaceOperation,
    },
}

/// 参数输入类型（决定提交后的操作构建）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpArgKind {
    RenameSession,
    NewWorkspace,
    RenameWorkspace,
    MoveSession,
}

/// 写操作回执（`workspace/*`/`session/*` 成功后的 apply 载荷）。
#[derive(Debug, Clone, PartialEq)]
pub enum OpOutcome {
    /// 无需特别处理的成功（rename/archive/delete/move）。
    Ack,
    /// fork 返回新会话 id（成功后打开）。
    ForkCreated { session_id: String },
    /// workspace/create 返回 id（刷新 workspace follow 对账）。
    WorkspaceCreated { workspace_id: String },
}

/// 命令面板候选条目（本地动作 / 远端斜杠命令 / V0.4 占位）。
#[derive(Debug, Clone, PartialEq)]
pub enum CommandPaletteItem {
    /// TUI 可执行动作（进入对应流程/发写命令）。
    Local {
        label: &'static str,
        desc: &'static str,
        action: PaletteAction,
    },
    /// 远端斜杠命令（`commands/list` 动态注册；执行经 commands/execute）。
    Remote { name: String, desc: String },
    /// V0.4 才有的项（settings/theme/keymap/export…），选中仅显示提示。
    V04 {
        label: &'static str,
        desc: &'static str,
    },
}

/// 命令面板 overlay 状态（REQ-006 §5 `CommandPaletteState`；仅内存）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandPaletteState {
    pub visible: bool,
    /// 输入行（命令名前缀 / 完整斜杠行）。
    pub query: String,
    /// 过滤后候选选中下标。
    pub selection: usize,
    /// 最近一次 `commands/execute` 失败 `error.code`（AC-006-13，D-035）。
    pub last_error_code: Option<String>,
    /// 最近一次执行反馈（成功/失败文本；面板内显示）。
    pub last_result: Option<String>,
    /// 远端命令表（`commands/list` 拉取一次缓存；打开会话后可用）。
    pub remote_commands: Vec<crate::api::types::CommandDescriptor>,
    /// 远端命令表是否已拉取（fetch-once）。
    pub remote_fetched: bool,
    /// `commands/execute` 单飞（同一行只提交一次，AC-006-12/15 同源）。
    pub executing: bool,
    /// Step 6 操作子阶段（参数输入 / 破坏性确认；None=命令列表）。
    pub stage: Option<PaletteStage>,
    /// workspace/session 写操作在途（request_id + label；requestId 幂等，
    /// AC-006-12——重复/迟到响应不重复 apply）。
    pub op_inflight: Option<(String, &'static str)>,
}

impl CommandPaletteState {
    /// 打开即重置为初态（保留远端缓存供重开复用；query/结果/stage 清空）。
    pub fn reset_for_open(&mut self) {
        self.visible = true;
        self.query.clear();
        self.selection = 0;
        self.last_error_code = None;
        self.last_result = None;
        self.executing = false;
        self.stage = None;
        self.op_inflight = None;
    }

    /// 进入参数输入子阶段（清空 query 作为输入缓冲）。
    pub fn begin_input(&mut self, kind: OpArgKind, prompt: &'static str) {
        self.stage = Some(PaletteStage::Input { prompt, kind });
        self.query.clear();
        self.selection = 0;
        self.last_result = None;
        self.last_error_code = None;
    }

    /// 进入破坏性二次确认。
    pub fn begin_confirm(&mut self, op: WorkspaceOperation) {
        let label = op.label().to_string();
        self.stage = Some(PaletteStage::ConfirmDanger { label, op });
        self.last_result = None;
        self.last_error_code = None;
    }

    /// 内置本地命令 + V0.4 占位（D-035 映射清单；workspace/session 操作项由
    /// sidebar 上下文与 Step 6 执行链路承载，此处列纯本地入口）。
    fn local_candidates() -> Vec<CommandPaletteItem> {
        vec![
            CommandPaletteItem::Local {
                label: "model catalog",
                desc: "模型目录与热切换（M）",
                action: PaletteAction::ModelCatalog,
            },
            CommandPaletteItem::Local {
                label: "new session",
                desc: "新建会话（官方 web New session）",
                action: PaletteAction::NewSession,
            },
            CommandPaletteItem::Local {
                label: "sidebar view",
                desc: "循环侧栏视图 groupBy/orderBy（gv）",
                action: PaletteAction::CycleSidebarView,
            },
            CommandPaletteItem::Local {
                label: "collapse all",
                desc: "折叠全部 workspace（h）",
                action: PaletteAction::CollapseAll,
            },
            CommandPaletteItem::Local {
                label: "expand all",
                desc: "展开全部 workspace（l）",
                action: PaletteAction::ExpandAll,
            },
            CommandPaletteItem::Local {
                label: "help",
                desc: "键位帮助（?）",
                action: PaletteAction::Help,
            },
            // ---------- REQ-006 workspace/session 操作（FR-006-02 操作半） ----------
            CommandPaletteItem::Local {
                label: "fork session",
                desc: "复制当前会话为分支（成功后打开）",
                action: PaletteAction::ForkSession,
            },
            CommandPaletteItem::Local {
                label: "rename session",
                desc: "重命名当前会话",
                action: PaletteAction::RenameSession,
            },
            CommandPaletteItem::Local {
                label: "archive session",
                desc: "归档当前会话（危险，二次确认）",
                action: PaletteAction::ArchiveSession,
            },
            CommandPaletteItem::Local {
                label: "move session",
                desc: "移动当前会话到目标 workspace",
                action: PaletteAction::MoveSession,
            },
            CommandPaletteItem::Local {
                label: "new workspace",
                desc: "新建 workspace（输入路径）",
                action: PaletteAction::NewWorkspace,
            },
            CommandPaletteItem::Local {
                label: "rename workspace",
                desc: "重命名光标所在 workspace",
                action: PaletteAction::RenameWorkspace,
            },
            CommandPaletteItem::Local {
                label: "delete workspace",
                desc: "删除光标所在 workspace（危险，二次确认）",
                action: PaletteAction::DeleteWorkspace,
            },
            CommandPaletteItem::Local {
                label: "settings",
                desc: "settings 白名单编辑（AC-007-15/16）",
                action: PaletteAction::OpenSettings,
            },
            CommandPaletteItem::Local {
                label: "skills",
                desc: "skills 目录只读 + 复制引用（AC-007-18）",
                action: PaletteAction::OpenSkills,
            },
            CommandPaletteItem::Local {
                label: "theme",
                desc: "主题切换 dark↔light（AC-007-20）",
                action: PaletteAction::ToggleTheme,
            },
            CommandPaletteItem::Local {
                label: "edit",
                desc: "用 $EDITOR 编辑当前草稿（AC-007-25；composer 打开时可用）",
                action: PaletteAction::EditWithEditor,
            },
            CommandPaletteItem::Local {
                label: "subagents",
                desc: "子代理目录（FR-007-01；需要活动会话）",
                action: PaletteAction::OpenSubagents,
            },
            CommandPaletteItem::Local {
                label: "goal",
                desc: "goal 面板（单例；create/edit/pause/resume/complete/clear）",
                action: PaletteAction::OpenGoal,
            },
            CommandPaletteItem::Local {
                label: "jobs",
                desc: "jobs 只读列表（官方无停止，指引 web）",
                action: PaletteAction::OpenJobs,
            },
            CommandPaletteItem::V04 {
                label: "keymap",
                desc: "键位编辑（V0.4）",
            },
            CommandPaletteItem::Local {
                label: "export",
                desc: "导出会话 ZIP（官方 /api/session.export）",
                action: PaletteAction::OpenExport,
            },
        ]
    }

    /// 全量候选（本地 + 远端斜杠）。远端需已拉取；否则仅本地。
    pub fn all_candidates(&self) -> Vec<CommandPaletteItem> {
        let mut out = Self::local_candidates();
        if self.remote_fetched {
            for c in &self.remote_commands {
                out.push(CommandPaletteItem::Remote {
                    name: c.name.clone(),
                    desc: c.description.clone(),
                });
            }
        }
        out
    }

    /// 过滤后的可见候选（UI 与 reducer 共享 seam：命令名/标签前缀匹配；
    /// 忽略前导 `:`/`/`，便于直接输入斜杠命令名）。
    pub fn filtered(&self) -> Vec<CommandPaletteItem> {
        let q = self
            .query
            .trim()
            .trim_start_matches(':')
            .trim_start_matches('/')
            .to_lowercase();
        self.all_candidates()
            .into_iter()
            .filter(|item| match item {
                CommandPaletteItem::Local { label, .. } => {
                    q.is_empty() || label.to_lowercase().contains(&q)
                }
                CommandPaletteItem::Remote { name, .. } => {
                    q.is_empty() || name.to_lowercase().contains(&q)
                }
                CommandPaletteItem::V04 { label, .. } => {
                    q.is_empty() || label.to_lowercase().contains(&q)
                }
            })
            .collect()
    }
}

/// turnOutline 大纲列表（`O`；D-19 独立键，与 `o` 打开不冲突）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutlineState {
    pub open: bool,
    pub selection: usize,
}

/// 轨迹内过滤输入态（`/`；本地 nucleo 窗口内过滤——即时、无异步防抖风暴，
/// AC-005-05/13）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrajFilterState {
    pub open: bool,
    pub query: String,
    /// 命中列表选中下标（j/k 于过滤列表；Enter 跳转该命中行）。
    pub cursor: usize,
}

/// Trajectory 视图状态（REQ-005 §5 状态机，D-25；全部内存、单写多读）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrajState {
    /// 折叠状态（纯数据；跨 gt/gT 切换保留）。
    pub fold: crate::model::FoldState,
    /// 选中行下标（视图行序；j/k 移动，gt/gT 保留）。
    pub cursor: usize,
    /// 详情子层开（D-25：Trajectory 内焦点子层，`Enter`/`d` 开）。
    pub detail_open: bool,
    /// 当前详情（打开时由 detail_for 构建；行变化/关闭失效，source 锚点防串）。
    pub detail: Option<crate::model::TrajectoryDetail>,
    /// 详情面板滚动偏移（j/k 于 Details 焦点时）。
    pub detail_scroll: usize,
    /// 轨迹内过滤（`/`）。
    pub filter: TrajFilterState,
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
    // ---------- REQ-006 V0.3 模型目录（FR-006-01） ----------
    /// `session/modelCatalog` 加载成功（generation 关联在途目录；stale 忽略）。
    ModelCatalogLoaded {
        generation: u64,
        catalog: crate::api::types::ModelCatalog,
    },
    /// `session/modelCatalog` 加载失败（目录区/状态条可观察，AC-006-06；
    /// 权限错误不自动重试，网络走既有重连）。
    ModelCatalogLoadFailed {
        generation: u64,
        error: ClientError,
    },
    /// `session/selectModel` 成功（服务端已确认 next；热切换生效，
    /// AC-006-08）。
    ModelSelected {
        selected: crate::api::types::WireModelSelection,
    },
    /// `session/selectModel` 失败（显示 error.code、当前模型不变、
    /// 权限错误不自动重试，AC-006-09）。
    ModelSelectFailed {
        error: ClientError,
    },
    // ---------- REQ-006 命令面板（FR-006-03） ----------
    /// `commands/list` 成功（agentId=当前会话 id，官方 wire `agentId:
    /// SessionId` 实读 0.1.2-rc.1）。
    RemoteCommandsLoaded {
        commands: Vec<crate::api::types::CommandDescriptor>,
    },
    /// `commands/list` 失败（面板内提示，不崩）。
    RemoteCommandsFailed {
        error: ClientError,
    },
    /// `commands/execute` 成功（result 文本；undefined 容忍为空成功）。
    CommandExecuted {
        text: Option<String>,
    },
    /// `commands/execute` 失败（显示 error.code，面板保持可继续输入，
    /// AC-006-13）。
    CommandExecuteFailed {
        error: ClientError,
    },
    // ---------- REQ-006 workspace/session 操作（FR-006-02 操作半） ----------
    /// `session/create` 成功（新建会话；刷新会话列表，web 立即可见一致，
    /// AC-006-02/04）。
    SessionCreated {
        session_id: String,
    },
    SessionCreateFailed {
        error: ClientError,
    },
    // ---------- REQ-006 workspace/session 操作回执（FR-006-02 操作半） ----------
    /// 写操作成功（requestId 校验：与在途不一致 = 重复/迟到响应，不 apply）。
    WorkspaceOpDone {
        request_id: String,
        outcome: OpOutcome,
    },
    /// 写操作失败（requestId 匹配才置错；本地不漂移，AC-006-10）。
    WorkspaceOpFailed {
        request_id: String,
        op_name: String,
        error: ClientError,
    },
    // ---------- REQ-007 V0.4 `:edit`（AC-007-25，prototype 验证） ----------
    /// 外部编辑器退出后的回执（main 释放/恢复 raw mode 并执行 $EDITOR）。
    ExternalEditDone {
        ok: bool,
        tmp_path: std::path::PathBuf,
        text: String,
        message: String,
    },
    // ---------- REQ-007 V0.4 @ 提及（AC-007-23） ----------
    /// `fileReferences/list` + `sessionReferenceResolver/candidates` 结果
    /// （generation 守卫：stale 丢弃）。
    MentionCandidates {
        generation: u64,
        files: Vec<crate::api::types::FileReferenceCandidate>,
        sessions: Vec<crate::api::types::SessionReferenceMentionCandidate>,
    },
    MentionCandidatesFailed {
        generation: u64,
        error: ClientError,
    },
    // ---------- REQ-007 V0.4 subagent 回执 ----------
    SubagentListed {
        parent_id: String,
        generation: u64,
        catalog: crate::api::types::SubagentCatalog,
    },
    SubagentListFailed {
        parent_id: String,
        generation: u64,
        error: ClientError,
    },
    SubagentInterruptDone {
        child_id: String,
        error: Option<ClientError>,
    },
    GoalOpDone {
        request_id: String,
        updated: Option<crate::api::types::GoalSnapshot>,
        cleared: bool,
    },
    GoalOpFailed {
        request_id: String,
        op: GoalMutation,
        error: ClientError,
    },
    SettingsDescribed {
        value: crate::api::types::SettingsDescribeValue,
    },
    SettingsDescribeFailed {
        error: ClientError,
    },
    SettingsUpdated {
        ns: String,
        view: crate::api::types::SettingsNamespaceView,
    },
    SettingsUpdateFailed {
        ns: String,
        error: ClientError,
    },
    SkillsListed {
        value: crate::api::types::SkillListValue,
    },
    SkillsListFailed {
        error: ClientError,
    },
    ExportDone {
        bytes: u64,
        path: std::path::PathBuf,
    },
    ExportFailed {
        error: ClientError,
    },
    // ---------- REQ-007 V0.4 消息动作回执 ----------
    MessageBranchDone {
        session_id: String,
    },
    MessageActionFailed {
        op: crate::model::MessageActionKind,
        error: ClientError,
    },
    FeedbackPutDone,
    FeedbackPutFailed {
        error: ClientError,
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
    // ---------- REQ-006 V0.3 模型目录（FR-006-01） ----------
    /// `session/modelCatalog` 拉取（打开 overlay 时一次；单飞 generation）。
    FetchModelCatalog {
        generation: u64,
    },
    /// `session/selectModel` 热切换（当前活动会话；reasoning effort 可选）。
    /// 幂等：reducer 以 `selecting` 单飞守卫，同一弹窗只提交一次（AC-006-15
    /// 同源；selectModel 请求无独立 requestId wire 字段，本地单飞足够）。
    SelectModel {
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    },
    // ---------- REQ-006 命令面板（FR-006-03） ----------
    /// `commands/list` 拉取（打开命令面板且有活动会话时一次；agentId=会话）。
    FetchRemoteCommands,
    /// `commands/execute` 执行斜杠行（agentId=会话 id；images 空）。
    ExecuteCommand {
        line: String,
    },
    /// `session/create` 新建会话（Step 5 `new session` 命令）。
    CreateSession,
    /// workspace/session 写操作（FR-006-02 操作半；requestId 幂等——
    /// 重复/迟到响应不重复 apply，AC-006-12）。
    WorkspaceOp {
        request_id: String,
        op: WorkspaceOperation,
    },
    // ---------- REQ-007 V0.4 ----------
    /// 主题切换后持久化 config.toml（AC-007-20；ADR-010 save_theme_config，
    /// 主循环执行同步 IO）。
    SaveUiTheme {
        theme: String,
        palette: std::collections::BTreeMap<String, String>,
    },
    /// `:edit` 外部编辑器（main 内联：TerminalSession 释放/恢复 raw mode +
    /// 前台运行 $EDITOR，AC-007-25）。
    ExternalEdit {
        tmp_path: std::path::PathBuf,
        editor: String,
    },
    /// `@` 两源候选拉取（fileReferences + sessionReferenceResolver；
    /// generation 单飞去重，AC-007-23）。
    FetchMentionCandidates {
        generation: u64,
        agent_id: String,
        query: String,
    },
    // ---------- REQ-007 V0.4 subagent（AC-007-07~10） ----------
    /// `subagents/list(parentId)` 拉取（generation 单飞）。
    FetchSubagentList {
        parent_id: String,
        generation: u64,
    },
    /// `subagents/interruptByParent`（位置参数：child/parent/mode）。
    SubagentInterrupt {
        child_id: String,
        parent_id: String,
    },
    /// `goals/*` 写操作（agentId=活动会话，CAS revision；requestId 幂等）。
    GoalOp {
        request_id: String,
        op: GoalMutation,
    },
    /// `settings/describe` 拉取（打开面板时一次）。
    FetchSettingsDescribe,
    /// `skills/list(sessionId)` 拉取。
    FetchSkillsList,
    /// `settings/update(ns, patch, expectedRevision)`（白名单 key 编辑 CAS）。
    SettingsUpdate {
        ns: String,
        key: String,
        value: serde_json::Value,
        revision: u64,
    },
    /// 会话导出：官方同源 HTTP `/api/session.export` 流式落盘。
    ExportSession {
        session_id: String,
        path: std::path::PathBuf,
    },
    // ---------- REQ-007 V0.4 消息动作（AC-007-27/28） ----------
    /// 分支：`session/fork atSeq`（静止轮次末条 user 消息）。
    ForkAtSeq {
        session_id: String,
        at_seq: u64,
    },
    /// feedback：`messageFeedback/put`（messageId=assistant message.id）。
    FeedbackPut {
        session_id: String,
        message_id: String,
        rating: String,
        note: Option<String>,
    },
}

#[derive(Debug)]
pub struct AppState {
    pub mode: Mode,
    pub focus: Focus,
    pub conn: ConnState,
    pub active_session: Option<SessionId>,
    pub sessions: SessionStore,
    /// REQ-005：多会话轨迹窗口缓存（最近 3，独立投影 D-23，不扩展
    /// `sessions` 的 TranscriptWindow）。
    pub traj_sessions: crate::model::TrajectoryStore,
    /// Trajectory 视图状态（折叠/选中/详情/过滤）。
    pub traj: TrajState,
    /// 轨迹内搜索索引（随过滤操作/窗口变化重建，本地 nucleo）。
    pub traj_search_index: crate::model::TrajectorySearchIndex,
    pub workspaces: WorkspaceStore,
    pub viewport: Viewport,
    pub picker: PickerState,
    pub composer: ComposerState,
    /// Active composer buffer (bound to a session; the registry keeps drafts
    /// across session switches, D-20).
    pub draft: Option<DraftState>,
    /// Cross-session draft registry (memory only, LRU 20).
    pub drafts: DraftRegistry,
    /// REQ-007 AC-007-22 / ADR-010: draft persistence switches + dirty flag.
    /// `drafts_enabled=false` degrades to the memory registry (REQ-003
    /// semantics); the main loop flushes on `take_draft_dirty()`.
    pub drafts_enabled: bool,
    /// 持久化目标路径（main 注入 `default_state_path()`）；None = 不落盘。
    pub drafts_path: Option<std::path::PathBuf>,
    draft_dirty: bool,
    /// Global input history (↑/↓, ≤50, memory only).
    pub history: InputHistory,
    /// 搜索 overlay + 窗口索引（随窗口重建）。
    pub search: SearchState,
    pub search_index: SearchIndex,
    /// 视觉选择/剪贴板。
    pub yank: YankState,
    /// 审批模态。
    pub approval: ApprovalState,
    /// 模型目录 overlay（REQ-006 FR-006-01，`M` 打开）。
    pub model_catalog: ModelCatalogState,
    /// 命令面板 overlay（REQ-006 FR-006-03，`:` 打开）。
    pub command_palette: CommandPaletteState,
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
    /// Details 列宽（config `[ui].details_width_cells` 注入；默认 45，
    /// clamp 30–60 由 layout 侧执行，Notes/04 §1）。
    pub details_width_cells: u16,
    /// Effective palette（REQ-007 AC-007-20；config `[ui] theme/palette`
    /// 注入；运行时 `:` theme 切换重绘并持久化，ADR-010）。
    pub palette: crate::ui::theme::Palette,
    /// User palette overrides kept on AppState (persisted on theme toggle).
    pub palette_overrides: std::collections::BTreeMap<String, String>,
    /// REQ-007 AC-007-21：生效 `[keymap]` 覆盖差异行（帮助面板联动；
    /// 空 = 内置键位无覆盖）。
    pub keymap_override_lines: Vec<String>,
    /// REQ-007 AC-007-25：外部编辑器挂起/回填状态机（`edit with $EDITOR`）。
    pub external_edit: crate::model::ExternalEditState,
    /// REQ-007 AC-007-23：@ 提及候选（files+sessions 两源）。
    pub mention: crate::model::MentionState,
    /// REQ-007 AC-007-24：本次发送在途的图片附件（submit 预检通过后暂存；
    /// 发送后清空）。
    pub pending_image_attachments: Vec<crate::model::ImageAttachment>,
    /// REQ-007 FR-007-01：subagent 目录树（AC-007-07~10）。
    pub subagents: crate::model::SubagentViewState,
    /// REQ-007 FR-007-02：goal 面板（单例 CAS；AC-007-11/12/14）。
    pub goals: crate::model::GoalPanelState,
    /// goal create/edit 输入子阶段是否激活（buffer 在 GoalPanelState）。
    pub goal_input: bool,
    /// REQ-007 AC-007-13：jobs 只读镜像（session/control 帧维护）。
    pub jobs: crate::model::JobsPanelState,
    /// REQ-007 AC-007-31：搜索 query 历史（上限 50 FIFO，最近在前）。
    pub query_history: crate::model::SearchHistory,
    /// REQ-007 AC-007-29：timeline 缩略条（`[ui].show_timeline` 控制）。
    pub timeline: crate::model::TimelineState,
    /// config `[ui].show_timeline`（默认 false）。
    pub show_timeline: bool,
    /// REQ-007 AC-007-27/28：消息动作菜单状态。
    pub message_action: crate::model::MessageActionState,
    /// 菜单目标（打开时捕获，防窗口漂移后错位）。
    pub msg_action_target: Option<MessageActionTarget>,
    /// REQ-007 AC-007-15/16：settings 面板。
    pub settings: crate::model::SettingsPanelState,
    /// REQ-007 AC-007-18：skills 目录。
    pub skills: crate::model::SkillsCatalogState,
    /// REQ-007 AC-007-17：会话导出。
    pub export: crate::model::ExportState,
    page_guard: PageGuard,
    /// 模型目录 fetch 单飞 generation（REQ-006 FR-006-01）：打开时自增，
    /// stale 响应（overlay 已关/已重开）直接丢弃（模式 15 in-flight 去重）。
    catalog_generation: u64,
    want_backfill: bool,
    /// `loadThrough(seq)` 在途目标：每页合并后 reducer 判断是否已覆盖，
    /// 未覆盖且仍有更多历史则继续发下一页（AC-003-09 按 seq 落位）。
    load_through_target: Option<SessionSeq>,
    load_through_pages: usize,
    running_sessions: HashSet<SessionId>,
    /// 侧栏本地视图态（REQ-006 D-034：group_by/order_by/折叠；gv 仅本地态，
    /// 无远端写）。折叠语义迁自 `collapsed_workspaces`（h 折叠 / l 展开）。
    pub sidebar_view: crate::model::WorkspaceViewState,
    /// 侧栏行光标（Focus::Sidebar 下 j/k 移动、Enter 打开）。
    pub sidebar: SidebarState,
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

/// REQ-007 AC-007-24：读取本地图片文件 → base64 inline ImageAttachment。
/// 只读真实本地文件（路径须经 image_path_lines 预筛，URL/引用不落此路径）。
/// 失败返回稳定可断言的中文错误串。
fn read_image_attachment(path: &str) -> Result<crate::model::ImageAttachment, String> {
    use base64::Engine as _;
    let mt = crate::model::image_attachment::media_type_from_path(path)?;
    let bytes = std::fs::read(path)
        .map_err(|e| format!("图片读取失败（{path}）: {e}——已保留草稿，可修正后重发"))?;
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(crate::model::ImageAttachment {
        path: path.to_string(),
        media_type: mt,
        data_base64,
        bytes: bytes.len(),
    })
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            mode: Mode::Normal,
            focus: Focus::Sidebar,
            conn: ConnState::Connecting,
            active_session: None,
            sessions: SessionStore::new(3),
            traj_sessions: crate::model::TrajectoryStore::new(3),
            traj: TrajState::default(),
            traj_search_index: crate::model::TrajectorySearchIndex::new(),
            workspaces: WorkspaceStore::new(),
            viewport: Viewport::default(),
            picker: PickerState::default(),
            composer: ComposerState::default(),
            draft: None,
            drafts: DraftRegistry::new(20),
            drafts_enabled: true,
            drafts_path: None,
            draft_dirty: false,
            history: InputHistory::new(50),
            search: SearchState::default(),
            search_index: SearchIndex::new(),
            yank: YankState::default(),
            approval: ApprovalState::default(),
            model_catalog: ModelCatalogState::default(),
            command_palette: CommandPaletteState::default(),
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
            details_width_cells: crate::ui::layout::DEFAULT_DETAILS_WIDTH,
            palette: crate::ui::theme::Palette::default(),
            palette_overrides: std::collections::BTreeMap::new(),
            keymap_override_lines: Vec::new(),
            external_edit: crate::model::ExternalEditState::default(),
            mention: crate::model::MentionState::default(),
            pending_image_attachments: Vec::new(),
            subagents: crate::model::SubagentViewState::default(),
            goals: crate::model::GoalPanelState::default(),
            goal_input: false,
            jobs: crate::model::JobsPanelState::default(),
            query_history: crate::model::SearchHistory::new(50),
            timeline: crate::model::TimelineState::default(),
            show_timeline: false,
            message_action: crate::model::MessageActionState::default(),
            msg_action_target: None,
            settings: crate::model::SettingsPanelState::default(),
            skills: crate::model::SkillsCatalogState::default(),
            export: crate::model::ExportState::default(),
            page_guard: PageGuard::default(),
            catalog_generation: 0,
            want_backfill: false,
            load_through_target: None,
            load_through_pages: 0,
            running_sessions: HashSet::new(),
            sidebar_view: crate::model::WorkspaceViewState::default(),
            sidebar: SidebarState::default(),
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

    // ---------- REQ-007 V0.4 theme (AC-007-20) ----------

    /// Apply config theme + palette overrides at startup (main loop injects
    /// `eff.ui`). Invalid overrides produce warnings (returned for the
    /// startup banner); never fatal.
    pub fn apply_palette_config(
        &mut self,
        theme: &str,
        palette: &std::collections::BTreeMap<String, String>,
    ) -> Vec<String> {
        self.palette_overrides = palette.clone();
        let built = crate::ui::theme::Palette::build(theme, palette);
        let warnings = built.warnings.clone();
        self.palette = built;
        warnings
    }

    /// `:` theme toggle: flip dark↔light, rebuild with current overrides.
    pub fn toggle_theme(&mut self) {
        let next = if self.palette.is_light() {
            "dark"
        } else {
            "light"
        };
        self.palette = crate::ui::theme::Palette::build(next, &self.palette_overrides);
        self.notice = Some(format!("主题: {next}"));
    }

    // ---------- REQ-007 V0.4 draft persistence (AC-007-22/ADR-010) ----------

    /// Mark the draft registry dirty so the main loop flushes drafts.toml.
    fn mark_drafts_dirty(&mut self) {
        if self.drafts_enabled {
            self.draft_dirty = true;
        }
    }

    /// Take the dirty flag (main loop polls it each iteration and flushes).
    pub fn take_draft_dirty(&mut self) -> bool {
        let dirty = self.draft_dirty;
        self.draft_dirty = false;
        dirty
    }

    /// Snapshot the registry into the on-disk DraftStore table.
    pub fn draft_store_snapshot(&self) -> crate::model::DraftStore {
        let mut store = crate::model::DraftStore::default();
        for sid in self.drafts.session_ids() {
            if let Some(d) = self.drafts.get(&sid) {
                store.set(&sid.0, &d.text);
            }
        }
        store
    }

    /// Seed the in-memory registry from a persisted DraftStore (startup /
    /// restore). Empty store → no-op (memory semantics unchanged).
    pub fn seed_drafts_from_store(&mut self, store: crate::model::DraftStore) {
        for (sid, text) in store.drafts {
            if !text.is_empty() {
                self.drafts.set(DraftState {
                    text,
                    cursor: 0,
                    bound_session: SessionId(sid),
                });
            }
        }
    }

    /// Clear all persisted + in-memory drafts (startup `[drafts].clear`).
    pub fn clear_all_drafts(&mut self) {
        self.drafts.clear_all();
        self.draft_dirty = true;
    }

    // ---------- REQ-007 V0.4 `:edit` 外部编辑器（AC-007-25，prototype ✅） ----------

    /// 解析 `$EDITOR`（`$VISUAL` 优先，回退 `$EDITOR`）；两者皆无 → None。
    fn resolve_editor() -> Option<String> {
        std::env::var("VISUAL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
    }

    /// 起始 `:edit`：把当前 composer 草稿写入 state 目录临时文件并挂起主循环
    /// （返回 Cmd::ExternalEdit，main 释放 raw mode → 前台 $EDITOR → 恢复）。
    pub fn external_edit_begin(&mut self) -> Vec<Cmd> {
        if !self.composer.visible {
            self.notice = Some("先打开 composer（i）再用 :edit 编辑草稿".into());
            return vec![];
        }
        let Some(sid) = self.composer.active_session.clone() else {
            self.notice = Some("无活动 composer 会话".into());
            return vec![];
        };
        let draft_text = self
            .draft
            .as_ref()
            .map(|d| d.text.clone())
            .unwrap_or_default();
        let Some(editor) = Self::resolve_editor() else {
            self.notice = Some("未设置 $EDITOR（export EDITOR=vim）".into());
            return vec![];
        };
        // 临时文件放 state 目录（~/.local/state/dshtui/，与主进程同文件系统；
        // 编辑器子进程同一进程内可见——规避 /tmp 跨调用坑，TASK-002-pitfall）。
        let base = crate::config::default_state_path();
        let dir = base
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.notice = Some(format!("无法创建 state 目录: {e}"));
            return vec![];
        }
        let tmp = dir.join(format!(
            "edit-{}-{}.md",
            sid.0,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        if let Err(e) = std::fs::write(&tmp, &draft_text) {
            self.notice = Some(format!("临时草稿写入失败: {e}"));
            return vec![];
        }
        // 挂起模型（capture 原草稿供失败恢复）+ 退 composer 模态（编辑器占用屏）。
        if !self.external_edit.suspend(
            &draft_text,
            tmp.to_string_lossy().into_owned(),
            Some(editor.clone()),
        ) {
            self.notice = Some("已有 :edit 在运行".into());
            return vec![];
        }
        self.mode = Mode::Normal;
        self.composer.visible = false;
        self.composer.active_session = None;
        vec![Cmd::ExternalEdit {
            tmp_path: tmp,
            editor,
        }]
    }

    // ---------- REQ-007 V0.4 subagent 目录（AC-007-07~10；FR-007-01） ----------

    /// `:subagents`：以活动会话为父打开目录并拉取直属 children。
    pub fn open_subagents(&mut self) -> Vec<Cmd> {
        let Some(sid) = self.active_session.clone() else {
            self.notice = Some("请先用 f/o 打开会话再查看子代理".into());
            return vec![];
        };
        self.subagents.open(&sid.0);
        self.mode = Mode::Subagent;
        self.fetch_subagent_list(sid.0.clone())
    }

    /// 拉取 `subagents/list(parent_id)`（generation 单飞）。
    fn fetch_subagent_list(&mut self, parent_id: String) -> Vec<Cmd> {
        self.subagents.loading = true;
        self.subagents.last_error_code = None;
        let gen = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        vec![Cmd::FetchSubagentList {
            parent_id,
            generation: gen,
        }]
    }

    /// 子代理目录命令分流（j/k 移动、Enter 展开/折叠、x 中断二次确认、
    /// Esc/q 关闭）。
    fn handle_subagent_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        if self.subagents.interrupt_target.is_some() {
            // 中断二次确认态：Enter 确认、其它键取消。
            match cmd {
                C::PickerConfirm => {
                    let (child, parent) = {
                        let child = self.subagents.interrupt_target.clone().unwrap();
                        let parent = self.subagents.parent_session_id.clone().unwrap_or_default();
                        (child, parent)
                    };
                    self.subagents.interrupt_target = None;
                    self.subagents.last_error_code = None;
                    return vec![Cmd::SubagentInterrupt {
                        child_id: child,
                        parent_id: parent,
                    }];
                }
                C::ClosePicker | C::PickerDown | C::PickerUp => {
                    self.subagents.interrupt_target = None;
                    return vec![];
                }
                _ => return vec![],
            }
        }
        match cmd {
            C::PickerDown => {
                let rows = self.subagents.flatten().len();
                if rows > 0 {
                    self.subagents.selected = (self.subagents.selected + 1).min(rows - 1);
                }
                vec![]
            }
            C::PickerUp => {
                self.subagents.selected = self.subagents.selected.saturating_sub(1);
                vec![]
            }
            C::PickerConfirm => {
                let Some(id) = self.subagents.selected_id() else {
                    return vec![];
                };
                // 展开/折叠（has_children）→ 需要时拉取。
                match self.subagents.toggle_expand(&id) {
                    Some((true, target)) => self.fetch_subagent_list(target),
                    _ => vec![],
                }
            }
            C::SubagentInterrupt => {
                let Some(id) = self.subagents.selected_id() else {
                    return vec![];
                };
                // 仅可中断 continuable/running child（非根/非诊断）。
                self.subagents.interrupt_target = Some(id);
                self.notice = Some("中断所选子代理？Enter 确认 / 其它键取消".into());
                vec![]
            }
            C::ClosePicker | C::Quit => {
                self.subagents.close();
                self.mode = Mode::Normal;
                vec![]
            }
            _ => vec![],
        }
    }

    /// 列表回执（generation 校验：过期丢弃）。
    pub fn subagents_listed(
        &mut self,
        parent_id: String,
        catalog: crate::api::types::SubagentCatalog,
    ) {
        self.subagents.set_catalog(&parent_id, catalog);
    }

    pub fn subagents_list_failed(&mut self, _parent_id: String, error: &ClientError) {
        self.subagents.loading = false;
        self.subagents.last_error_code = Some(error.code());
    }

    /// 中断回执：成功/失败均清目标；失败展示 error.code（权限不自动重试）。
    pub fn subagents_interrupt_done(&mut self, child_id: String, error: Option<&ClientError>) {
        if let Some(e) = error {
            self.subagents.last_error_code = Some(e.code());
            self.notice = Some(format!("中断子代理 {child_id} 失败: {e}"));
        } else {
            self.subagents.last_error_code = None;
            self.notice = Some(format!("已请求中断子代理 {child_id}"));
        }
    }

    // ---------- REQ-007 V0.4 goal 面板（AC-007-11/12/14；FR-007-02 half） ----------

    /// `:goal`：打开面板并读取当前活动会话 goal 投影（单例）。
    pub fn open_goal_panel(&mut self) -> Vec<Cmd> {
        if self.active_session.is_none() {
            self.notice = Some("请先用 f/o 打开会话再查看 goal".into());
            return vec![];
        }
        self.goals.open();
        self.goal_input = false;
        self.mode = Mode::Goal;
        if self.active_window().is_none() {
            self.goals.set_goal(None, false);
        } else {
            self.refresh_goal_from_projection();
        }
        vec![]
    }

    /// 从活动窗口 projections 读取 `goal` 投影刷新面板（ADR-008 只读，
    /// 缺字段/Null → 空态）。
    pub fn refresh_goal_from_projection(&mut self) {
        // 无窗口时不做刷新（保留当前 state：stale 需等真实投影回 fresh，
        // 空态在 open_goal_panel 已设置）。
        let Some(window) = self.active_window() else {
            return;
        };
        let snap = crate::model::ProjectionSnapshot::new(window.projections().clone());
        match snap.goal() {
            Some(gv) => self.goals.set_goal(Some(gv), false),
            None => self.goals.set_goal(None, false),
        }
    }

    /// goal 面板命令分流。
    fn handle_goal_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        // 输入子阶段（create/edit objective）。
        if self.goal_input {
            match cmd {
                C::PickerInput(t) => {
                    self.goals.create_objective.push_str(&t);
                    vec![]
                }
                C::PickerBackspace => {
                    self.goals.create_objective.pop();
                    vec![]
                }
                C::PickerConfirm => {
                    let objective = std::mem::take(&mut self.goals.create_objective);
                    self.goal_input = false;
                    if objective.trim().is_empty() {
                        self.notice = Some("goal objective 不能为空".into());
                        return vec![];
                    }
                    let op = if self.goals.goal.is_none() {
                        GoalMutation::Create {
                            objective,
                            max_goal_rounds: None,
                        }
                    } else {
                        GoalMutation::Edit { objective }
                    };
                    self.send_goal_op(op)
                }
                C::ClosePicker | C::Quit => {
                    self.goal_input = false;
                    self.goals.create_objective.clear();
                    vec![]
                }
                _ => vec![],
            }
        } else {
            match cmd {
                C::GoalCreate => {
                    if self.goals.goal.is_some() {
                        self.notice = Some("已有 goal（单例）；先 clear 再重建".into());
                        return vec![];
                    }
                    self.goal_input = true;
                    self.goals.create_objective.clear();
                    vec![]
                }
                C::GoalEdit => {
                    if self.goals.goal.is_none() {
                        return vec![];
                    }
                    let obj = self
                        .goals
                        .goal
                        .as_ref()
                        .map(|g| g.objective.clone())
                        .unwrap_or_default();
                    self.goals.create_objective = obj;
                    self.goal_input = true;
                    vec![]
                }
                C::GoalPause => self.send_goal_op(GoalMutation::Pause),
                C::GoalResume => self.send_goal_op(GoalMutation::Resume),
                C::GoalComplete => self.send_goal_op(GoalMutation::Complete),
                C::GoalClear => {
                    // clear 二次确认（ConfirmDanger 先例）。
                    if self.goals.goal.is_none() {
                        return vec![];
                    }
                    self.goals.request_confirm(crate::model::GoalOpKind::Clear);
                    self.notice = Some("clear 当前 goal？Enter 确认 / 其它键取消".into());
                    vec![]
                }
                C::PickerConfirm => {
                    // clear 确认态。
                    if self.goals.confirm_pending == Some(crate::model::GoalOpKind::Clear) {
                        return self.send_goal_op(GoalMutation::Clear);
                    }
                    vec![]
                }
                C::ClosePicker | C::Quit => {
                    self.goals.close();
                    self.mode = Mode::Normal;
                    vec![]
                }
                _ => vec![],
            }
        }
    }

    /// 发起 goal 写操作：CAS revision=当前投影；单飞拒绝；stale 拒绝重读。
    fn send_goal_op(&mut self, op: GoalMutation) -> Vec<Cmd> {
        let kind = op.op_kind();
        if kind != crate::model::GoalOpKind::Create && self.goals.goal.is_none() {
            return vec![];
        }
        // Create 走空态（goal None）。
        if !self.goals.begin_op(kind) {
            self.notice = Some("goal 操作在途或 revision stale——先重读投影".into());
            return vec![];
        }
        let request_id = crate::api::types::mint_request_id();
        vec![Cmd::GoalOp { request_id, op }]
    }

    /// goal 写操作成功回执（requestId 匹配才 apply）。
    pub fn goal_op_done(
        &mut self,
        request_id: &str,
        updated: Option<crate::api::types::GoalSnapshot>,
        cleared: bool,
    ) {
        if !self.goal_inflight_matches(request_id) {
            return;
        }
        if cleared {
            self.goals.settle_op(None);
            self.notice = Some("goal 已 clear".into());
            return;
        }
        if let Some(snap) = updated {
            // 回执可能带回更新后快照（宽容）；随后投影帧也会刷新。
            self.goals.settle_op(Some(crate::model::GoalView {
                id: snap.id,
                revision: snap.revision,
                objective: snap.objective,
                phase: snap.phase,
                blocked_reason: snap.blocked_reason,
                max_goal_rounds: snap.max_goal_rounds,
                ..Default::default()
            }));
            self.notice = Some(format!("goal {}", snap.phase.map(|_| "更新").unwrap_or("")));
        } else {
            self.goals.settle_op(None);
            self.refresh_goal_from_projection();
        }
    }

    fn goal_inflight_matches(&self, _request_id: &str) -> bool {
        // goal 单例串行：begin_op 保证 ≤1 在途，任一在途回执即当前 op
        // （无列表并发，requestId 槽冗余）。
        self.goals.inflight.is_some()
    }

    /// goal 写操作失败：GOAL_STALE_REVISION → stale 重读；其它 error.code
    /// 展示，权限不自动重试。
    pub fn goal_op_failed(&mut self, request_id: &str, op: &GoalMutation, error: &ClientError) {
        if !self.goal_inflight_matches(request_id) {
            return;
        }
        let stale = error.code().contains("STALE") || error.code().contains("CONFLICT");
        self.goals.fail_op(error.code(), stale);
        if stale {
            self.notice = Some("goal revision 过期——已重读投影，可重试".into());
            self.refresh_goal_from_projection();
        } else {
            self.notice = Some(format!("goal {} 失败: {error}", op.op_kind().as_str()));
        }
    }

    // ---------- REQ-007 V0.4 jobs 只读（AC-007-13） ----------

    pub fn open_jobs_panel(&mut self) -> Vec<Cmd> {
        self.jobs.open();
        self.mode = Mode::Jobs;
        vec![]
    }

    fn handle_jobs_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        match cmd {
            C::PickerDown => {
                self.jobs.move_selection(1);
                vec![]
            }
            C::PickerUp => {
                self.jobs.move_selection(-1);
                vec![]
            }
            C::ClosePicker | C::Quit => {
                self.jobs.close();
                self.mode = Mode::Normal;
                vec![]
            }
            _ => vec![],
        }
    }

    /// 从 control 帧更新 jobs 镜像（全量替换语义：baseline.jobs / `jobs`
    /// 替换帧；空数组清镜像——不伪造数字，ADR-008）。
    pub fn jobs_control_item(&mut self, session_id: &SessionId, item: &ControlItem) {
        match item {
            ControlItem::Baseline { jobs, .. } => {
                let jobs = crate::api::session::parse_jobs(jobs);
                if !jobs.is_empty() {
                    self.jobs.replace(jobs);
                } else if self.jobs.visible {
                    // baseline 无 jobs（未运行）→ 面板仍显示空态。
                    self.jobs.replace(Vec::new());
                }
                let _ = session_id;
            }
            ControlItem::Jobs { jobs } => {
                self.jobs.replace(crate::api::session::parse_jobs(jobs));
            }
            _ => {}
        }
    }

    // ---------- REQ-007 V0.4 settings + skills（AC-007-15~19） ----------

    /// 白名单可编辑 key 集（镜像 web UI；描述树只读展示其余标量）。
    pub const SETTINGS_WHITELIST: [&'static str; 7] = [
        "locale.preference",
        "ui-theme.preference",
        "ui-theme.fontSize",
        "ui-chat.transcriptView",
        "ui-conversation.busyEnter",
        "agent-presets.default",
        "permission.defaultPreset",
    ];

    pub fn open_settings_panel(&mut self) -> Vec<Cmd> {
        self.settings.open();
        self.mode = Mode::Settings;
        vec![Cmd::FetchSettingsDescribe]
    }

    pub fn settings_described(&mut self, value: crate::api::types::SettingsDescribeValue) {
        let mut rows = Vec::new();
        for ns in &value.namespaces {
            let user = ns.user.as_ref();
            rows.extend(crate::model::flatten_namespace_rows(
                &ns.ns,
                &ns.value,
                user,
                &ns.secrets,
                ns.revision,
                &Self::SETTINGS_WHITELIST,
            ));
        }
        self.settings.set_rows(rows, value.writable);
    }

    pub fn settings_describe_failed(&mut self, error: &ClientError) {
        self.settings.loading = false;
        self.settings.last_error_code = Some(error.code());
    }

    pub fn settings_updated(
        &mut self,
        _ns: &str,
        _view: &crate::api::types::SettingsNamespaceView,
    ) {
        self.settings.edit_key = None;
        self.settings.edit_buffer.clear();
        self.notice = Some("settings 已更新（expectedRevision CAS）".into());
        // 重新 describe 拉新 revision（视图刷新）。
        self.settings.loading = true;
    }

    pub fn settings_update_failed(&mut self, _ns: &str, error: &ClientError) {
        self.settings.last_error_code = Some(error.code());
        self.notice = Some(format!("settings 更新失败: {error}"));
        // CAS 冲突：清编辑态要求重拉 describe（不自动重试）。
        if error.code().contains("CONFLICT") || error.code().contains("STALE") {
            self.settings.edit_key = None;
        }
    }

    /// settings 面板命令分流：编辑子阶段（Enter 提交/字符/Backspace/Esc）；
    /// 列表态 j/k 移动、Enter 进编辑、Esc/q 关闭。
    fn handle_settings_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        if self.settings.edit_key.is_some() {
            match cmd {
                C::PickerInput(t) => {
                    self.settings.edit_buffer.push_str(&t);
                    vec![]
                }
                C::PickerBackspace => {
                    self.settings.edit_buffer.pop();
                    vec![]
                }
                C::PickerConfirm => {
                    let (ns, key, value, rev) = {
                        let row = self
                            .settings
                            .rows
                            .iter()
                            .find(|r| Some(r.key.as_str()) == self.settings.edit_key.as_deref())
                            .cloned();
                        let Some(row) = row else {
                            return vec![];
                        };
                        let mut parts = row.key.splitn(2, '.');
                        let ns = parts.next().unwrap_or("").to_string();
                        let key = parts.next().unwrap_or("").to_string();
                        let value = serde_json::Value::String(self.settings.edit_buffer.clone());
                        (ns, key, value, row.revision)
                    };
                    self.settings.edit_key = None;
                    self.settings.edit_buffer.clear();
                    vec![Cmd::SettingsUpdate {
                        ns,
                        key,
                        value,
                        revision: rev,
                    }]
                }
                C::ClosePicker | C::Quit => {
                    self.settings.edit_key = None;
                    self.settings.edit_buffer.clear();
                    vec![]
                }
                _ => vec![],
            }
        } else {
            match cmd {
                C::PickerDown => {
                    self.settings.move_selection(1);
                    vec![]
                }
                C::PickerUp => {
                    self.settings.move_selection(-1);
                    vec![]
                }
                C::PickerConfirm => {
                    let Some(row) = self.settings.rows.get(self.settings.selected).cloned() else {
                        return vec![];
                    };
                    if !self.settings.writable || row.secret {
                        self.notice = Some("该 key 只读展示（白名单外/secret 不可编辑）".into());
                        return vec![];
                    }
                    self.settings.edit_key = Some(row.key);
                    self.settings.edit_buffer = row.value_display.clone();
                    vec![]
                }
                C::ClosePicker | C::Quit => {
                    self.settings.close();
                    self.mode = Mode::Normal;
                    vec![]
                }
                _ => vec![],
            }
        }
    }

    pub fn open_skills_panel(&mut self) -> Vec<Cmd> {
        self.skills.open();
        self.mode = Mode::Skills;
        vec![Cmd::FetchSkillsList]
    }

    pub fn skills_listed(&mut self, value: crate::api::types::SkillListValue) {
        self.skills.set_items(value.skills);
    }

    pub fn skills_list_failed(&mut self, error: &ClientError) {
        self.skills.loading = false;
        self.skills.last_error_code = Some(error.code());
    }

    /// skills 面板命令分流：j/k 移动、y 复制引用、Esc/q 关闭。
    fn handle_skills_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        match cmd {
            C::PickerDown => {
                self.skills.move_selection(1);
                vec![]
            }
            C::PickerUp => {
                self.skills.move_selection(-1);
                vec![]
            }
            C::YankContext => {
                if let Some(ref_text) = self.skills.copy_ref() {
                    self.notice = Some(format!("已复制 {ref_text}（执行走 / 斜杠入口）"));
                    vec![Cmd::CopyToClipboard { text: ref_text }]
                } else {
                    vec![]
                }
            }
            C::ClosePicker | C::Quit => {
                self.skills.close();
                self.mode = Mode::Normal;
                vec![]
            }
            _ => vec![],
        }
    }

    // ---------- REQ-007 V0.4 会话导出（AC-007-17；官方 HTTP 路由） ----------

    pub fn open_export_panel(&mut self) -> Vec<Cmd> {
        let Some(sid) = self.active_session.clone() else {
            self.notice = Some("请先用 f/o 打开会话再导出".into());
            return vec![];
        };
        let default = format!("dshtui-export-{}.zip", sid.0);
        self.export.open(&sid.0, &default);
        self.mode = Mode::Export;
        vec![]
    }

    fn handle_export_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        match cmd {
            C::PickerInput(t) => {
                if self.export.phase == crate::model::export::ExportPhase::PickingPath {
                    self.export.path.push_str(&t);
                }
                vec![]
            }
            C::PickerBackspace => {
                if self.export.phase == crate::model::export::ExportPhase::PickingPath {
                    self.export.path.pop();
                }
                vec![]
            }
            C::PickerConfirm => {
                if !self.export.begin_download() {
                    self.notice = Some("路径为空或在途".into());
                    return vec![];
                }
                let Some(sid) = self.export.session_id.clone() else {
                    return vec![];
                };
                let path = std::path::PathBuf::from(self.export.path.clone());
                vec![Cmd::ExportSession {
                    session_id: sid,
                    path,
                }]
            }
            C::ClosePicker | C::Quit => {
                if self.export.phase == crate::model::export::ExportPhase::Downloading
                    || self.export.phase == crate::model::export::ExportPhase::Rebuilding
                {
                    self.export.cancelled = true;
                    self.notice = Some("导出已取消——在途下载完成后临时文件自动清理".into());
                }
                self.export.close();
                self.mode = Mode::Normal;
                vec![]
            }
            _ => vec![],
        }
    }

    pub fn export_done(&mut self, bytes: u64, path: &std::path::Path) {
        self.export.mark_progress(bytes);
        self.export.finish();
        self.notice = Some(format!("导出完成: {}（{} 字节）", path.display(), bytes));
    }

    pub fn export_failed(&mut self, error: &ClientError) {
        self.export.fail(error.code());
        self.notice = Some(format!("导出失败: {error}"));
        if error.class() == crate::api::envelope::ErrorClass::PermissionDenied {
            tracing::error!(error = %error, "导出权限不足");
        }
    }

    // ---------- REQ-007 V0.4 消息动作（AC-007-27/28） ----------

    /// Normal 模式 `m`：以 cursor_block 为锚打开动作菜单。
    pub fn open_message_actions(&mut self) -> Vec<Cmd> {
        let Some(sid) = self.active_session.clone() else {
            self.notice = Some("无活动会话".into());
            return vec![];
        };
        let Some(window) = self.active_window() else {
            return vec![];
        };
        let blocks = window.block_snapshot();
        let Some(block) = blocks.get(self.cursor_block) else {
            return vec![];
        };
        let seq = block.seq().0;
        let running = self.active_running();
        // 是否为「静止轮次末条 user」：cursor 块是 user 且其后无 user。
        let is_last_user = match block {
            crate::model::Block::UserMessage { .. } => blocks[self.cursor_block + 1..]
                .iter()
                .all(|b| !matches!(b, crate::model::Block::UserMessage { .. })),
            _ => false,
        };
        let (kind, user_text, message_id) = match block {
            crate::model::Block::UserMessage { content, .. } => {
                (MsgTargetKind::User, Some(content.clone()), None)
            }
            crate::model::Block::AssistantMessage { message_id, .. } => {
                (MsgTargetKind::Assistant, None, message_id.clone())
            }
            _ => {
                self.notice = Some("该消息行不支持动作（仅 user/assistant 消息）".into());
                return vec![];
            }
        };
        self.message_action.open_menu(seq);
        self.msg_action_target = Some(MessageActionTarget {
            session_id: sid,
            seq,
            kind,
            user_text,
            message_id,
            running,
            is_last_user,
        });
        self.mode = Mode::MessageAction;
        vec![]
    }

    /// 菜单可用动作（据目标块 + wire 语义）：assistant → feedback±；
    /// user → retry（重发）+ branch（仅静止轮次末条）。
    fn msg_available_actions(&self) -> Vec<crate::model::MessageActionKind> {
        use crate::model::MessageActionKind as K;
        let Some(t) = &self.msg_action_target else {
            return vec![];
        };
        match t.kind {
            MsgTargetKind::Assistant => {
                if t.message_id.is_some() {
                    vec![K::FeedbackPositive, K::FeedbackNegative]
                } else {
                    vec![]
                }
            }
            MsgTargetKind::User => {
                let mut v = vec![K::Retry];
                if t.is_last_user && !t.running {
                    v.push(K::Branch);
                }
                v
            }
        }
    }

    fn handle_message_action_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        match cmd {
            C::PickerDown | C::PickerUp => {
                if self.message_action.confirm_running {
                    // 二次确认态锁定动作，忽略光标移动。
                    return vec![];
                }
                let n = self.msg_available_actions().len();
                if n == 0 {
                    return vec![];
                }
                if cmd == C::PickerDown {
                    self.message_action.menu_cursor =
                        (self.message_action.menu_cursor + 1).min(n - 1);
                } else {
                    self.message_action.menu_cursor =
                        self.message_action.menu_cursor.saturating_sub(1);
                }
                vec![]
            }
            C::PickerConfirm => {
                let Some(action) = self
                    .msg_available_actions()
                    .get(self.message_action.menu_cursor)
                    .copied()
                else {
                    return vec![];
                };
                let Some(target) = self.msg_action_target.clone() else {
                    return vec![];
                };
                // 二次确认态：Enter = 确认执行之前选中的动作（AC-007-28）。
                if self.message_action.confirm_running {
                    if self.message_action.confirm(action) {
                        return self.execute_message_action(action, &target);
                    }
                    return vec![];
                }
                // 单飞：running 目标先置二次确认；静态直接执行。
                if !self.message_action.begin(action, target.running) {
                    return vec![];
                }
                if self.message_action.confirm_running {
                    self.notice = Some("该轮正在运行——Enter 确认执行 / 其它键取消".into());
                    return vec![];
                }
                self.execute_message_action(action, &target)
            }
            C::ClosePicker | C::Quit => {
                if self.message_action.confirm_running {
                    self.message_action.fail("cancelled".into());
                }
                self.message_action.settle();
                self.msg_action_target = None;
                self.mode = Mode::Normal;
                vec![]
            }
            _ => vec![],
        }
    }

    fn execute_message_action(
        &mut self,
        action: crate::model::MessageActionKind,
        target: &MessageActionTarget,
    ) -> Vec<Cmd> {
        match action {
            crate::model::MessageActionKind::Branch => {
                self.message_action.settle();
                self.msg_action_target = None;
                self.mode = Mode::Normal;
                vec![Cmd::ForkAtSeq {
                    session_id: target.session_id.0.clone(),
                    at_seq: target.seq,
                }]
            }
            crate::model::MessageActionKind::Retry => {
                let Some(text) = target.user_text.clone() else {
                    self.message_action.fail("no-user-text".into());
                    return vec![];
                };
                if text.trim().is_empty() {
                    self.message_action.fail("empty".into());
                    return vec![];
                }
                self.message_action.settle();
                self.msg_action_target = None;
                self.mode = Mode::Normal;
                // 乐观回显 + 重发 prompt（新 requestId，wire 校正：retry=重发）。
                let request_id = SessionRequestId(crate::api::types::mint_request_id());
                let sid = target.session_id.clone();
                self.sessions
                    .touch(&sid.0, self.window_cap)
                    .echo(request_id.clone(), &text);
                self.notice = Some("已重发该消息（retry）".into());
                let request = PromptRequest {
                    request_id,
                    session_id: sid.clone(),
                    mode: PromptMode::Queue,
                    content: vec![PromptContentPart::Text { text }],
                    client_time_zone: None,
                };
                vec![Cmd::SendPrompt {
                    session_id: sid,
                    request,
                }]
            }
            kind @ (crate::model::MessageActionKind::FeedbackPositive
            | crate::model::MessageActionKind::FeedbackNegative) => {
                let Some(mid) = target.message_id.clone() else {
                    self.message_action.fail("no-message-id".into());
                    return vec![];
                };
                self.message_action.settle();
                self.msg_action_target = None;
                self.mode = Mode::Normal;
                let rating = if kind == crate::model::MessageActionKind::FeedbackPositive {
                    "positive"
                } else {
                    "negative"
                };
                vec![Cmd::FeedbackPut {
                    session_id: target.session_id.0.clone(),
                    message_id: mid,
                    rating: rating.to_string(),
                    note: None,
                }]
            }
        }
    }

    // ---- 回执 ----
    pub fn message_branch_done(&mut self, session_id: String) -> Vec<Cmd> {
        self.notice = Some(format!("已创建分支会话 {session_id}"));
        self.list_loaded = false;
        let mut cmds = vec![Cmd::LoadSessionList { cursor: None }];
        cmds.extend(self.open_session(SessionId(session_id)));
        cmds
    }

    pub fn message_action_failed(
        &mut self,
        op: crate::model::MessageActionKind,
        error: &ClientError,
    ) {
        self.message_action.fail(error.code());
        self.notice = Some(format!("{} 失败: {error}", op.as_str()));
    }

    pub fn feedback_put_done(&mut self) {
        self.notice = Some("feedback 已提交".into());
    }

    pub fn feedback_put_failed(&mut self, error: &ClientError) {
        self.message_action.fail(error.code());
        self.notice = Some(format!("feedback 提交失败: {error}"));
    }

    // ---------- REQ-007 V0.4 @ 提及（AC-007-23；model/mention + api/references） ----------

    /// `@` 词边界判定：@ 前是空或空白（不在路径/单词中间）。
    fn mention_boundary(draft: &str) -> bool {
        draft
            .chars()
            .last()
            .map(|c| c.is_whitespace())
            .unwrap_or(true)
    }

    /// INSERT 输入 '@' → 激活提及（仅当词边界）。返回激活伴随的拉取命令
    /// （空 = 未激活）。
    pub fn maybe_activate_mention(&mut self) -> Vec<Cmd> {
        if self.mention.active {
            return vec![];
        }
        let boundary = self
            .draft
            .as_ref()
            .map(|d| Self::mention_boundary(&d.text))
            .unwrap_or(true);
        if !boundary {
            return vec![];
        }
        self.mention.activate();
        self.mode = Mode::Mention;
        self.request_mention_fetch()
    }

    /// 提及模态命令分流（模式接管：字符进 query、j/k 移动、Enter 回填、
    /// Esc/q 关闭、Backspace）。
    fn handle_mention_command(&mut self, cmd: crate::input::Command) -> Option<Vec<Cmd>> {
        use crate::input::Command as C;
        match cmd {
            C::PickerInput(text) => {
                let q = format!("{}{}", self.mention.query, text);
                self.mention.set_query(q);
                Some(self.request_mention_fetch())
            }
            C::PickerBackspace => {
                self.mention.query.pop();
                Some(self.request_mention_fetch())
            }
            C::PickerDown => {
                self.mention.move_selection(1);
                Some(vec![])
            }
            C::PickerUp => {
                self.mention.move_selection(-1);
                Some(vec![])
            }
            C::PickerConfirm => {
                let candidate: Option<crate::model::MentionCandidate> = {
                    let rows = self.mention.filtered();
                    rows.get(self.mention.selected).map(|c| (*c).clone())
                };
                let Some(candidate) = candidate else {
                    self.mention.deactivate();
                    self.mode = Mode::Insert;
                    return Some(vec![]);
                };
                self.mention.deactivate();
                self.mode = Mode::Insert;
                if let Some(d) = self.draft.as_mut() {
                    if !d.text.is_empty() && !d.text.ends_with(' ') && !d.text.ends_with('@') {
                        d.text.push(' ');
                    }
                    d.text.push_str(&candidate.insert);
                    if !d.text.ends_with(' ') {
                        d.text.push(' ');
                    }
                    d.cursor = d.text.chars().count();
                }
                self.notice = Some(format!("@ 已插入: {}", candidate.insert));
                Some(vec![])
            }
            C::ClosePicker => {
                self.mention.deactivate();
                self.mode = Mode::Insert;
                Some(vec![])
            }
            // 其它命令：关闭提及落回 INSERT，交由主 match 继续（返回 None）。
            _ => None,
        }
    }

    /// 请求两源候选（generation 单飞；active_session 为 agentId）。
    fn request_mention_fetch(&mut self) -> Vec<Cmd> {
        let Some(sid) = self.composer.active_session.clone() else {
            return vec![];
        };
        let query = self.mention.query.clone();
        self.mention.mark_loading();
        let gen = self.mention.generation;
        vec![Cmd::FetchMentionCandidates {
            generation: gen,
            agent_id: sid.0.clone(),
            query,
        }]
    }

    /// 两源结果回填（stale 丢弃）。
    pub fn mention_candidates(
        &mut self,
        generation: u64,
        files: Vec<crate::api::types::FileReferenceCandidate>,
        sessions: Vec<crate::api::types::SessionReferenceMentionCandidate>,
    ) {
        self.mention.set_candidates(generation, files, sessions);
    }

    pub fn mention_candidates_failed(&mut self, generation: u64, error: &ClientError) {
        if generation == self.mention.generation {
            self.mention.fail(error.code());
        }
    }

    /// 编辑器退出回执：成功 → 回填 composer 并恢复 INSERT；失败 → 保留原草稿
    /// 安全回 composer（可读错误）。
    pub fn external_edit_done(
        &mut self,
        ok: bool,
        tmp_path: &std::path::Path,
        text: &str,
        message: &str,
    ) {
        let _ = std::fs::remove_file(tmp_path); // 会话内清理临时文件
        let sid = self
            .draft
            .as_ref()
            .map(|d| d.bound_session.clone())
            .or_else(|| self.composer.active_session.clone());
        if ok {
            self.external_edit.settle();
            self.draft = Some(DraftState {
                text: text.to_string(),
                cursor: text.chars().count(),
                bound_session: sid.unwrap_or_else(|| SessionId(String::new())),
            });
            self.mode = Mode::Insert;
            self.composer.visible = true;
            self.composer.active_session = self.draft.as_ref().map(|d| d.bound_session.clone());
            self.notice = Some("外部编辑器内容已回填 composer".into());
        } else {
            self.external_edit.fail(message.to_string());
            // 保留原草稿（suspended_text 为空则新空草稿），恢复 INSERT。
            let restored = self.external_edit.suspended_text.clone();
            if let Some(sid) = sid {
                self.draft = Some(DraftState {
                    text: restored,
                    cursor: 0,
                    bound_session: sid,
                });
                self.mode = Mode::Insert;
                self.composer.visible = true;
                self.composer.active_session = self.draft.as_ref().map(|d| d.bound_session.clone());
            }
            self.last_error = Some(format!(":edit 失败: {message}"));
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
                // AC-006-17：approval/policy ask|never 只读展示（宽容解析，
                // 无 TUI 切换入口 D-037）。逐快照刷新：最新投影为准。
                if let Some(p) = projections.as_ref() {
                    self.approval.policy_display = crate::api::approval::policy_hint(p);
                }
                // REQ-007：goal 投影逐快照刷新（面板打开时实时；ADR-008
                // 只读官方 goal）。
                if self.mode == Mode::Goal
                    && self
                        .active_session
                        .as_ref()
                        .is_some_and(|s| s == &session_id)
                {
                    self.refresh_goal_from_projection();
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
                        records: records.clone(),
                        has_more,
                        projections: projections.clone(),
                    })
                };
                // REQ-005：同一快照喂独立轨迹投影（D-23，边界事件全保留）。
                {
                    let tw = self.traj_sessions.touch(&session_id.0, self.window_cap);
                    tw.apply(TrajIncoming::Snapshot {
                        cursor,
                        records,
                        has_more,
                        projections,
                    });
                }
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
                let eff = {
                    let Some(w) = self.sessions.get_mut(&session_id.0) else {
                        tracing::warn!(session = %session_id, "事件到达但窗口不存在，丢弃");
                        return vec![];
                    };
                    w.apply(Incoming::FollowEvent(event.clone()))
                };
                // REQ-005：同一事件喂轨迹投影（边界事件不丢，D-23）。
                if let Some(tw) = self.traj_sessions.get_mut(&session_id.0) {
                    tw.apply(TrajIncoming::FollowEvent(event));
                }
                self.adjust_viewport(&eff);
                self.window_changed();
                vec![]
            }
            AppEvent::FollowChunks { session_id, row } => {
                let eff = {
                    let Some(w) = self.sessions.get_mut(&session_id.0) else {
                        return vec![];
                    };
                    w.apply(Incoming::Chunks(row.clone()))
                };
                if let Some(tw) = self.traj_sessions.get_mut(&session_id.0) {
                    tw.apply(TrajIncoming::Chunks(row));
                }
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
                let eff = self.sessions.get_mut(&session_id.0).map(|w| {
                    w.apply(Incoming::Page {
                        records: records.clone(),
                        has_more,
                    })
                });
                if let Some(eff) = eff {
                    self.adjust_viewport(&eff);
                    self.window_changed();
                }
                // REQ-005：同一页喂轨迹投影（前插合并无重复无空洞，AC-005-07）。
                if let Some(tw) = self.traj_sessions.get_mut(&session_id.0) {
                    tw.apply(TrajIncoming::Page { records, has_more });
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
                        self.mark_drafts_dirty();
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
                // 队列去重（pending/active/failed/granted 命中 → false 忽略；
                // 模式 15 重放防护，AC-006-15/18）。danger 标记为宽容读取
                // （api 层 needs_ack；wire `[未验证]`）。
                // AC-006-17：帧自身携带 policy（ask|never）→ 只读展示。
                if let Some(policy) = crate::api::approval::policy_hint(&event.raw) {
                    self.approval.policy_display = Some(policy);
                }
                let danger = crate::api::approval::needs_ack(&event.raw);
                if !self.approval.queue.enqueue(event, danger) {
                    tracing::debug!("重复审批事件忽略（队列去重）");
                    return vec![];
                }
                // 无活动项（首次到达 / 上一项已清）→ promote 进单条槽显示。
                if self.approval.event.is_none() {
                    self.approval_promote_display();
                }
                vec![]
            }
            AppEvent::ApprovalReplied { outcome } => {
                self.approval.reply_inflight = false;
                self.approval.last_outcome = Some(outcome);
                self.approval.toast = Some(format!("审批已回复: {}", outcome.as_str()));
                let granted = outcome == ApprovalOutcome::AllowedOnce;
                // settle 后队列自动 promote 下一 pending（≤1 在途）。
                self.approval.queue.settle(granted);
                // 批量模式：非 danger 停点 → 自动续发下一项（AC-006-15
                // 串行泵；逐条 unary，无服务端批量端点）。
                let batch_cmds = self.approval_pump_if_batch();
                if self.approval.queue.has_active() {
                    // 展示下一项（danger 停点等 ack 时禁用批量）。
                    self.approval_sync_from_queue();
                    if self.approval.queue.head_requires_ack() {
                        self.approval.batch_allow = false;
                        self.approval.toast = Some(
                            "危险操作需先确认风险：按 a 确认后再允许（仍仅授权本次）".to_string(),
                        );
                    }
                } else if self.approval.queue.summary().failed > 0 {
                    // pending/active 已清但仍有失败项：留在 Approval 并打开
                    // 列表视图（`r` 重试失败项入口，AC-006-14）。
                    self.approval.event = None;
                    self.approval.batch_allow = false;
                    self.approval.list_open = true;
                    self.approval.list_cursor = 0;
                    self.approval.waiting_hint = false;
                    self.approval.toast =
                        Some("部分审批失败（已保留）：列表 [r] 重试失败项 / [q] 退出".to_string());
                } else {
                    // 队列全空 → 关闭弹窗恢复先前模式（AC-003 单条行为不变）。
                    // 单条路径最常见的 toast 此刻弹窗已关不可见：转状态条
                    // notice（Step 7 ① approval.toast 死数据修复），并清残留
                    // 防下次打开带出陈旧文案。
                    let closing_toast = self.approval.toast.take();
                    self.approval.event = None;
                    self.approval.visible = false;
                    self.approval.waiting_hint = false;
                    self.approval.batch_allow = false;
                    self.approval.list_open = false;
                    if let Some(text) = closing_toast {
                        self.notice = Some(text);
                    }
                    self.mode = self.approval.prev_mode;
                }
                batch_cmds
            }
            AppEvent::ApprovalReplyFailed { outcome, error } => {
                // fail closed：不授权；本条保留为失败项可单独重试（AC-006-14
                // 部分失败语义），其余队列项自动续发继续处理。
                self.approval.reply_inflight = false;
                self.approval.queue.fail_active();
                if self.approval.queue.has_active() {
                    self.approval_sync_from_queue();
                    let batch_cmds = self.approval_pump_if_batch();
                    self.last_error = Some(format!(
                        "审批回复失败（{}，不授权）: {error}；失败项可 [L] 列表重试",
                        outcome.as_str()
                    ));
                    return batch_cmds;
                }
                // 无剩余项：收缩为状态条 `等待审批` + 指引官方 web
                // （AC-003-17/18），失败项保留可 [L] 列表重试。
                self.approval.event = None;
                self.approval.visible = false;
                self.approval.waiting_hint = true;
                self.approval.batch_allow = false;
                self.mode = self.approval.prev_mode;
                self.last_error = Some(format!(
                    "审批回复失败（{}，不授权）: {error}；请在官方 web 完成审批（失败项可 [L] 列表重试）",
                    outcome.as_str()
                ));
                vec![]
            }
            AppEvent::ControlItem { session_id, item } => {
                // REQ-007：jobs 镜像维护（AC-007-13；只读，无停止控制）。
                self.jobs_control_item(&session_id, &item);
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
                    // AC-006-17：approval/policy ask|never 只读展示。
                    self.approval.policy_display = crate::api::approval::policy_hint(&projections);
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
            // ---------- REQ-006：模型目录（FR-006-01） ----------
            AppEvent::ModelCatalogLoaded {
                generation,
                catalog,
            } => {
                // stale（overlay 已关/已重开后迟到）直接丢弃（模式 15）。
                if generation != self.catalog_generation || !self.model_catalog.visible {
                    tracing::debug!(generation, "modelCatalog stale 响应丢弃");
                    return vec![];
                }
                self.model_catalog.index.rebuild(&catalog);
                self.model_catalog.phase = CatalogPhase::Ready;
                self.model_catalog.load_error = None;
                self.model_catalog.selection = 0;
                self.model_catalog.query.clear();
                // ADR-008：current/next 只读官方 modelSelection 投影镜像。
                self.model_catalog.current_model = self
                    .active_window()
                    .map(|w| ProjectionSnapshot::new(w.projections().clone()))
                    .and_then(|p| p.model_selection().last_used);
                self.model_catalog.next_model = self
                    .active_window()
                    .map(|w| ProjectionSnapshot::new(w.projections().clone()))
                    .and_then(|p| p.model_selection().next);
                vec![]
            }
            AppEvent::ModelCatalogLoadFailed { generation, error } => {
                if generation != self.catalog_generation || !self.model_catalog.visible {
                    tracing::debug!(generation, "modelCatalog stale 失败丢弃");
                    return vec![];
                }
                self.model_catalog.phase = CatalogPhase::Error;
                let code = error.code();
                // code() 各变体均非空（envelope.rs）；统一含 code 展示。
                self.model_catalog.load_error =
                    Some(format!("模型目录加载失败: {error}（error.code={code}）"));
                // 权限错误不自动重试（只记录，不置 last_error 干扰重连提示）；
                // 网络断开走既有重连（既有 open follow 流驱动）。
                if error.class() == ErrorClass::PermissionDenied {
                    tracing::error!(error = %error, "模型目录权限不足");
                } else {
                    tracing::warn!(error = %error, "模型目录加载失败");
                }
                vec![]
            }
            AppEvent::ModelSelected { selected } => {
                self.model_catalog.selecting = None;
                self.model_catalog.phase = CatalogPhase::Ready;
                self.model_catalog.effort = None;
                self.model_catalog.next_model = Some(selected.display());
                self.model_catalog.visible = false;
                self.mode = Mode::Normal;
                self.notice = Some(format!(
                    "模型已切换: {}（下一次 prompt 生效）",
                    selected.display()
                ));
                vec![]
            }
            AppEvent::ModelSelectFailed { error } => {
                // 当前使用模型不变（不本地改）；显示 error.code，权限错误不
                // 自动重试（AC-006-09）。目录保持打开可继续选/退出。
                self.model_catalog.selecting = None;
                self.model_catalog.phase = CatalogPhase::Ready;
                let code = error.code();
                self.model_catalog.last_error_code = Some(code.clone());
                self.model_catalog.load_error = Some(format!("模型切换失败: {error}"));
                if error.class() == ErrorClass::PermissionDenied {
                    tracing::error!(error = %error, "selectModel 权限不足");
                } else {
                    tracing::warn!(error = %error, "selectModel 失败");
                }
                vec![]
            }
            // ---------- REQ-006 命令面板（FR-006-03） ----------
            AppEvent::RemoteCommandsLoaded { commands } => {
                // 缓存命令表（fetch-once；重开命令面板复用）。
                self.command_palette.remote_commands = commands;
                self.command_palette.remote_fetched = true;
                self.command_palette.selection = 0;
                vec![]
            }
            AppEvent::RemoteCommandsFailed { error } => {
                self.command_palette.last_error_code = Some(error.code());
                self.command_palette.last_result = Some(format!("斜杠命令加载失败: {error}"));
                tracing::warn!(error = %error, "commands/list 失败");
                vec![]
            }
            AppEvent::CommandExecuted { text } => {
                // execute 单飞结束；面板显示结果文本，保持打开可继续输入
                // （AC-006-13 不崩溃）。
                self.command_palette.executing = false;
                self.command_palette.last_result = Some(match text {
                    Some(t) if !t.trim().is_empty() => t,
                    _ => "命令已执行".to_string(),
                });
                vec![]
            }
            AppEvent::CommandExecuteFailed { error } => {
                // AC-006-13：显示 error.code、面板不崩溃可继续输入、权限错误
                // 不自动重试。
                self.command_palette.executing = false;
                let code = error.code();
                self.command_palette.last_error_code = Some(code);
                self.command_palette.last_result = Some(format!("命令执行失败: {error}"));
                if error.class() == ErrorClass::PermissionDenied {
                    tracing::error!(error = %error, "commands/execute 权限不足");
                } else {
                    tracing::warn!(error = %error, "commands/execute 失败");
                }
                vec![]
            }
            // ---------- REQ-006 workspace/session 操作（FR-006-02 操作半） ----------
            AppEvent::SessionCreated { session_id } => {
                // 新建成功：notice + 重拉会话列表（web 一致，AC-006-02/04）。
                self.notice = Some(format!("已新建会话 {session_id}"));
                self.list_loaded = false;
                vec![Cmd::LoadSessionList { cursor: None }]
            }
            AppEvent::SessionCreateFailed { error } => {
                let code = error.code();
                self.command_palette.last_error_code = Some(code);
                self.command_palette.last_result = Some(format!("新建会话失败: {error}"));
                if error.class() == ErrorClass::PermissionDenied {
                    tracing::error!(error = %error, "session/create 权限不足");
                } else {
                    tracing::warn!(error = %error, "session/create 失败");
                }
                vec![]
            }
            // ---------- REQ-006 workspace/session 操作回执（FR-006-02） ----------
            AppEvent::WorkspaceOpDone {
                request_id,
                outcome,
            } => {
                // requestId 幂等（AC-006-12）：与在途不匹配 = 重复/迟到响应，
                // 不重复 apply。
                let Some((inflight_id, label)) = self.command_palette.op_inflight.clone() else {
                    return vec![];
                };
                if inflight_id != request_id {
                    tracing::debug!(request_id, "workspace op 重复/迟到响应忽略（幂等）");
                    return vec![];
                }
                self.command_palette.op_inflight = None;
                match outcome {
                    OpOutcome::ForkCreated { session_id } => {
                        // fork 成功：打开新会话（web 列表经重拉一致）。
                        let sid = SessionId(session_id.clone());
                        self.notice = Some(format!("已创建分支会话 {session_id}"));
                        self.list_loaded = false;
                        let mut cmds = vec![Cmd::LoadSessionList { cursor: None }];
                        cmds.extend(self.open_session(sid));
                        cmds
                    }
                    OpOutcome::WorkspaceCreated { workspace_id } => {
                        self.notice = Some(format!("已新建 workspace {workspace_id}"));
                        // workspace/follow 增量对账；重拉会话列表确保一致。
                        self.list_loaded = false;
                        vec![Cmd::LoadSessionList { cursor: None }]
                    }
                    OpOutcome::Ack => {
                        self.notice = Some(format!("操作成功: {label}（web 端同步中）"));
                        // 本地列表不经手改（不漂移）；重拉会话列表与 web 对账
                        // （AC-006-02 操作后 web 立即可见一致）。
                        self.list_loaded = false;
                        vec![Cmd::LoadSessionList { cursor: None }]
                    }
                }
            }
            AppEvent::WorkspaceOpFailed {
                request_id,
                op_name,
                error,
            } => {
                // 仅当与在途匹配才落错（迟到失败不覆盖新状态）。
                if self
                    .command_palette
                    .op_inflight
                    .as_ref()
                    .is_some_and(|(rid, _)| *rid == request_id)
                {
                    self.command_palette.op_inflight = None;
                    let code = error.code();
                    self.command_palette.last_error_code = Some(code);
                    self.command_palette.last_result = Some(format!("{op_name} 失败: {error}"));
                    // AC-006-10：本地列表不漂移（未收到成功前不改本地）；
                    // 权限错误不自动重试。
                    if error.class() == ErrorClass::PermissionDenied {
                        tracing::error!(error = %error, "{op_name} 权限不足");
                    } else {
                        tracing::warn!(error = %error, "{op_name} 失败");
                    }
                }
                vec![]
            }
            // ---------- REQ-007 V0.4 `:edit` 回执（AC-007-25） ----------
            AppEvent::ExternalEditDone {
                ok,
                tmp_path,
                text,
                message,
            } => {
                self.external_edit_done(ok, &tmp_path, &text, &message);
                vec![]
            }
            // ---------- REQ-007 V0.4 @ 提及回执（AC-007-23） ----------
            AppEvent::MentionCandidates {
                generation,
                files,
                sessions,
            } => {
                self.mention_candidates(generation, files, sessions);
                vec![]
            }
            AppEvent::MentionCandidatesFailed { generation, error } => {
                self.mention_candidates_failed(generation, &error);
                vec![]
            }
            // ---------- REQ-007 V0.4 subagent 回执 ----------
            AppEvent::SubagentListed {
                parent_id, catalog, ..
            } => {
                self.subagents_listed(parent_id, catalog);
                vec![]
            }
            AppEvent::SubagentListFailed {
                parent_id, error, ..
            } => {
                self.subagents_list_failed(parent_id, &error);
                vec![]
            }
            AppEvent::SubagentInterruptDone { child_id, error } => {
                self.subagents_interrupt_done(child_id, error.as_ref());
                vec![]
            }
            // ---------- REQ-007 V0.4 goal 回执 ----------
            AppEvent::GoalOpDone {
                request_id,
                updated,
                cleared,
            } => {
                self.goal_op_done(&request_id, updated, cleared);
                vec![]
            }
            AppEvent::GoalOpFailed {
                request_id,
                op,
                error,
            } => {
                self.goal_op_failed(&request_id, &op, &error);
                vec![]
            }
            // ---------- REQ-007 V0.4 settings + skills 回执 ----------
            AppEvent::SettingsDescribed { value } => {
                self.settings_described(value);
                vec![]
            }
            AppEvent::SettingsDescribeFailed { error } => {
                self.settings_describe_failed(&error);
                vec![]
            }
            AppEvent::SettingsUpdated { ns, view } => {
                self.settings_updated(&ns, &view);
                // 更新成功 → 重拉 describe 同步 revision。
                vec![Cmd::FetchSettingsDescribe]
            }
            AppEvent::SettingsUpdateFailed { ns, error } => {
                self.settings_update_failed(&ns, &error);
                vec![]
            }
            AppEvent::SkillsListed { value } => {
                self.skills_listed(value);
                vec![]
            }
            AppEvent::SkillsListFailed { error } => {
                self.skills_list_failed(&error);
                vec![]
            }
            AppEvent::ExportDone { bytes, path } => {
                self.export_done(bytes, &path);
                vec![]
            }
            AppEvent::ExportFailed { error } => {
                self.export_failed(&error);
                vec![]
            }
            // ---------- REQ-007 V0.4 消息动作回执 ----------
            AppEvent::MessageBranchDone { session_id } => self.message_branch_done(session_id),
            AppEvent::MessageActionFailed { op, error } => {
                self.message_action_failed(op, &error);
                vec![]
            }
            AppEvent::FeedbackPutDone => {
                self.feedback_put_done();
                vec![]
            }
            AppEvent::FeedbackPutFailed { error } => {
                self.feedback_put_failed(&error);
                vec![]
            }
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
        // REQ-007 AC-007-29：本地窗口事件 → timeline 标记（无远端读取）。
        if self.show_timeline {
            use crate::model::timeline::TimelineMarkerKind as K;
            let kinds: Vec<K> = blocks
                .iter()
                .filter_map(|b| match b {
                    crate::model::Block::UserMessage { .. } => Some(K::User),
                    crate::model::Block::AssistantMessage { .. } => Some(K::Assistant),
                    crate::model::Block::ToolCall { .. }
                    | crate::model::Block::ToolResult { .. } => Some(K::Tool),
                    _ => None,
                })
                .collect();
            self.timeline.rebuild(&kinds);
        }
        if self.search.open {
            self.recompute_window_matches();
        }
        // REQ-005：轨迹搜索索引随事件流窗口变化重建（流式过滤中新到事件立
        // 即进命中，AC-005-05「过滤即时」；借用分离见 traj_index_rebuild）。
        if self.traj.filter.open && self.mode == Mode::Trajectory {
            self.traj_index_rebuild();
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
            self.mark_drafts_dirty();
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
    /// REQ-007 AC-007-24：整行本地图片路径（supported ext）→ 读取+base64 →
    /// Image part（顺序 `[images..., text]`）；任何图片候选读取/超限失败 →
    /// 保留草稿在 INSERT，可读提示，不自动重试。
    pub fn submit_input(&mut self, mode: PromptMode) -> Vec<Cmd> {
        if self.mode != Mode::Insert || !self.composer.visible {
            return vec![];
        }
        let Some(sid) = self.composer.active_session.clone() else {
            return vec![];
        };
        let draft_text = {
            let Some(d) = self.draft.as_mut() else {
                return vec![];
            };
            if d.text.trim().is_empty() {
                // Empty input: send nothing, stay in INSERT (AC-002-03).
                return vec![];
            }
            d.text.clone()
        };
        // ---------- AC-007-24：图片附件预检（失败保留草稿不发送） ----------
        let image_lines = crate::model::image_attachment::image_path_lines(&draft_text);
        if !image_lines.is_empty() {
            let mut attachments: Vec<crate::model::ImageAttachment> = Vec::new();
            for path in image_lines {
                match read_image_attachment(path) {
                    Ok(att) => attachments.push(att),
                    Err(msg) => {
                        self.notice = Some(msg);
                        return vec![]; // 保留草稿在 INSERT
                    }
                }
            }
            // 官方 imageLimits 校验（投影缺省 → 无限制不强制）。
            let limits = self.active_window().map(|w| {
                crate::model::ProjectionSnapshot::new(w.projections().clone()).image_limits()
            });
            let state = crate::model::ImageAttachmentState {
                pending: attachments.clone(),
                inflight: false,
                last_error_code: None,
            };
            let err = state.validate(
                limits
                    .as_ref()
                    .and_then(|l| l.max_image_bytes.map(|v| v as usize)),
                limits
                    .as_ref()
                    .and_then(|l| l.max_images_per_message.map(|v| v as usize)),
                if limits.as_ref().is_some_and(|l| !l.media_types.is_empty()) {
                    Some(&limits.as_ref().unwrap().media_types)
                } else {
                    None
                },
            );
            if let Err(msg) = err {
                self.notice = Some(msg);
                return vec![]; // 保留草稿在 INSERT（不自动重试）
            }
            self.pending_image_attachments = attachments;
        }
        // take-once + single command queue = minimal in-flight guard
        // (pattern 15 lesson: unconverged async signals need in-flight
        // dedup; AC-002-13 blocks double-Enter).
        let text = {
            let Some(d) = self.draft.as_mut() else {
                return vec![];
            };
            d.cursor = 0;
            std::mem::take(&mut d.text)
        };
        self.mode = Mode::Normal;
        self.composer.visible = false;
        self.composer.steer = false;
        self.composer.active_session = None;
        // 发送后清空该会话草稿（AC-003-11）+ 记入输入历史（AC-003-10）。
        self.drafts.clear(&sid);
        self.mark_drafts_dirty();
        self.history.push(&text);
        self.history.reset_nav();
        let request_id = SessionRequestId(crate::api::types::mint_request_id());
        // Optimistic echo: visible within one frame, occupies no seq
        // (AC-002-02). 有图片时 echo 保留文本摘要（图片路径不展开）。
        let echo_text = if self.pending_image_attachments.is_empty() {
            text.clone()
        } else {
            let n = self.pending_image_attachments.len();
            let base = text
                .lines()
                .filter(|l| {
                    crate::model::image_attachment::image_path_lines(&text)
                        .iter()
                        .all(|p| l.trim() != *p)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if base.trim().is_empty() {
                format!("[{} 张图片]", n)
            } else {
                format!("[{} 张图片] {}", n, base.trim())
            }
        };
        self.sessions
            .touch(&sid.0, self.window_cap)
            .echo(request_id.clone(), &echo_text);
        // content 顺序 `[image parts..., text]`（官方 web）。
        let mut content: Vec<PromptContentPart> = Vec::new();
        for att in std::mem::take(&mut self.pending_image_attachments) {
            content.push(PromptContentPart::Image {
                media_type: att.media_type.0,
                data: att.data_base64,
                name: std::path::Path::new(&att.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
            });
        }
        let text_part = text
            .lines()
            .filter(|l| {
                crate::model::image_attachment::image_path_lines(&text)
                    .iter()
                    .all(|p| l.trim() != *p)
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text_part.trim().is_empty() {
            content.push(PromptContentPart::Text { text: text_part });
        }
        if content.is_empty() {
            // 全部行都是图片但读取为空不应发生（前面已校验）；兜底纯文本。
            content.push(PromptContentPart::Text { text });
        }
        let request = PromptRequest {
            request_id: request_id.clone(),
            session_id: sid.clone(),
            mode,
            content,
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
        // REQ-005：Trajectory 模式命令分流（模态上下文：详情子层 q 关面板、
        // 列表焦点 q 退出；D-25）。
        if self.mode == Mode::Trajectory {
            if let Some(cmds) = self.handle_trajectory_command(cmd.clone()) {
                return cmds;
            }
        }
        // REQ-007：@ 提及模态命令分流（AC-007-23；字符/导航/确认/关闭）。
        // 返回 None = 提及已关闭且命令应交由既有 mode 路径继续处理。
        if self.mode == Mode::Mention {
            if let Some(cmds) = self.handle_mention_command(cmd.clone()) {
                return cmds;
            }
            self.mode = Mode::Insert; // 落回 INSERT 后再走主 match
        }
        // REQ-007：subagent 目录模态命令分流（AC-007-07~10）。
        if self.mode == Mode::Subagent {
            return self.handle_subagent_command(cmd);
        }
        // REQ-007：goal 面板命令分流（AC-007-11/12/14）。
        if self.mode == Mode::Goal {
            return self.handle_goal_command(cmd);
        }
        // REQ-007：jobs 只读面板（AC-007-13）。
        if self.mode == Mode::Jobs {
            return self.handle_jobs_command(cmd);
        }
        // REQ-007：消息动作菜单（AC-007-27/28）。
        if self.mode == Mode::MessageAction {
            return self.handle_message_action_command(cmd);
        }
        // REQ-007：settings / skills（AC-007-15~19）。
        if self.mode == Mode::Settings {
            return self.handle_settings_command(cmd);
        }
        if self.mode == Mode::Skills {
            return self.handle_skills_command(cmd);
        }
        // REQ-007：export（AC-007-17）。
        if self.mode == Mode::Export {
            return self.handle_export_command(cmd);
        }
        // REQ-007 AC-007-31：SEARCH 编辑态空 query 时 ↑/↓ = 历史回忆（非空时
        // 箭头保持既有 'k'/'j' 输入语义）。
        if self.mode == Mode::Search && !self.search.results_locked {
            use crate::input::Command as C2;
            // ↑ 回忆：空 query（起始）或正处于回忆游标（连续回看）。
            let recalling = self.search.recall_cursor.is_some();
            if matches!(cmd, C2::PickerUp) && (self.search.query.trim().is_empty() || recalling) {
                return self.search_recall_older();
            }
            if recalling && matches!(cmd, C2::PickerDown) {
                return self.search_recall_newer();
            }
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
                } else if self.mode == Mode::Normal
                    && self.focus == Focus::Sidebar
                    && matches!(cmd, C::MoveDown | C::MoveUp)
                {
                    // REQ-006（D-034/Step 4 Prototype PASS）：Focus::Sidebar 下
                    // j/k 移动侧栏行光标（不触发 Chat 滚动）；Chat 滚动需在
                    // Center/Details 焦点（Ctrl+w 循环）。半页/G/gg 仍是滚动。
                    if cmd == C::MoveDown {
                        self.sidebar_cursor_move(true);
                    } else {
                        self.sidebar_cursor_move(false);
                    }
                    vec![]
                } else {
                    self.scroll(cmd)
                }
            }
            C::SubagentInterrupt => {
                // 仅 subagent 模态上下文有意义（已在 handle_subagent_command
                // 分流）；此处兜底 no-op。
                vec![]
            }
            C::GoalCreate
            | C::GoalEdit
            | C::GoalPause
            | C::GoalResume
            | C::GoalComplete
            | C::GoalClear => {
                // 仅 goal 模态有意义（已分流）；兜底 no-op。
                vec![]
            }
            C::OpenMessageActions => {
                // Normal 模式焦点消息行打开动作菜单（其它模态 no-op）。
                if self.mode == Mode::Normal {
                    return self.open_message_actions();
                }
                vec![]
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
                Mode::Approval if self.approval.list_open => {
                    // Esc 在审批列表 = 回单条槽不中止（REQ-006 D-036）。
                    self.approval_close_list()
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
                // REQ-005：Trajectory Esc（详情/过滤关闭）已由分流处理；
                // 此处保持穷尽性。
                Mode::Trajectory => vec![],
                // REQ-006：模型目录 q/Esc 关闭（effort 子阶段先退一级回主列表，
                // 再按一次才关闭整个目录）。
                Mode::ModelCatalog => {
                    if self.model_catalog.effort.is_some() {
                        self.model_catalog.effort = None;
                        self.model_catalog.selection = 0;
                    } else {
                        self.close_model_catalog();
                    }
                    vec![]
                }
                // REQ-006：命令面板 q/Esc——输入/确认子阶段先取消回列表，
                // 无子阶段才关闭面板。
                Mode::CommandPalette => {
                    if self.command_palette.stage.is_some() {
                        self.command_palette.stage = None;
                        self.command_palette.query.clear();
                        self.command_palette.selection = 0;
                    } else {
                        self.close_command_palette();
                    }
                    vec![]
                }
                // REQ-007：@ 提及 Esc 已在 handle_mention_command 拦截
                // （此 arm 不可达，保穷尽性）。
                Mode::Mention => vec![],
                // REQ-007：subagent Esc 已在 handle_subagent_command 拦截。
                Mode::Subagent => vec![],
                // REQ-007：goal Esc 已在 handle_goal_command 拦截。
                Mode::Goal => vec![],
                // REQ-007：jobs Esc 已在 handle_jobs_command 拦截。
                Mode::Jobs => vec![],
                // REQ-007：settings/skills/export/message-action Esc 已分流。
                Mode::Settings => vec![],
                Mode::Skills => vec![],
                Mode::Export => vec![],
                Mode::MessageAction => vec![],
            },
            C::PickerDown => {
                if self.approval.list_open && self.mode == Mode::Approval {
                    // REQ-006 审批列表：j/k 移动光标。
                    let total = self.approval.queue.len();
                    if total > 0 {
                        self.approval.list_cursor = (self.approval.list_cursor + 1).min(total - 1);
                    }
                    vec![]
                } else if self.mode == Mode::Search {
                    if !self.search.results_locked {
                        // 编辑段：j 是输入字符。
                        return self.search_input("j");
                    }
                    let total = self.search.history_hits.len();
                    if total > 0 {
                        self.search.history_selection =
                            (self.search.history_selection + 1).min(total - 1);
                    }
                    vec![]
                } else if self.mode == Mode::ModelCatalog {
                    self.model_catalog_cursor_move(true);
                    vec![]
                } else if self.mode == Mode::CommandPalette {
                    self.command_palette_cursor_move(true);
                    vec![]
                } else {
                    self.picker.selection += 1;
                    vec![]
                }
            }
            C::PickerUp => {
                if self.approval.list_open && self.mode == Mode::Approval {
                    // REQ-006 审批列表：j/k 移动光标。
                    self.approval.list_cursor = self.approval.list_cursor.saturating_sub(1);
                    vec![]
                } else if self.mode == Mode::Search {
                    if !self.search.results_locked {
                        // 编辑段：k 是输入字符。
                        return self.search_input("k");
                    }
                    self.search.history_selection = self.search.history_selection.saturating_sub(1);
                    vec![]
                } else if self.mode == Mode::ModelCatalog {
                    self.model_catalog_cursor_move(false);
                    vec![]
                } else if self.mode == Mode::CommandPalette {
                    self.command_palette_cursor_move(false);
                    vec![]
                } else {
                    self.picker.selection = self.picker.selection.saturating_sub(1);
                    vec![]
                }
            }
            C::PickerInput(text) => {
                if self.mode == Mode::Insert {
                    // REQ-007：`@` 词边界触发提及候选（AC-007-23）。
                    if text == "@" {
                        let activation = self.maybe_activate_mention();
                        if !activation.is_empty() {
                            activation
                        } else {
                            self.composer_input(&text)
                        }
                    } else {
                        self.composer_input(&text)
                    }
                } else if self.mode == Mode::Search {
                    self.search_input(&text)
                } else if self.mode == Mode::ModelCatalog {
                    self.model_catalog_input(&text);
                    vec![]
                } else if self.mode == Mode::CommandPalette {
                    self.command_palette_input(&text);
                    vec![]
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
                } else if self.mode == Mode::ModelCatalog {
                    self.model_catalog_backspace();
                    vec![]
                } else if self.mode == Mode::CommandPalette {
                    self.command_palette_backspace();
                    vec![]
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
                        // REQ-006：Sidebar `o` 打开光标行（session→open；
                        // workspace header→折叠/展开）。
                        Focus::Sidebar => self.open_sidebar_cursor_row(),
                        _ => self.open_external_at_cursor(),
                    }
                }
            }
            C::OpenFocused => {
                // NORMAL Enter：Center 图片打开；REQ-006 Sidebar Enter 打开
                // 光标行（session/workspace）。
                if self.focus == Focus::Sidebar {
                    self.open_sidebar_cursor_row()
                } else if self.focus == Focus::Center && self.focused_image_block().is_some() {
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
                // h：折叠所有 workspace（本地视图态，D-034）。
                self.sidebar_view.collapse_all(&self.workspaces);
                self.clamp_sidebar_cursor();
                vec![]
            }
            C::ExpandProject => {
                // l：展开全部 workspace。
                self.sidebar_view.expand_all();
                self.clamp_sidebar_cursor();
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
                // REQ-006：模型目录 Enter——主列表：选中模型（带 efforts →
                // 进 effort 子阶段）；effort 子阶段：确认 effort 提交热切换。
                Mode::ModelCatalog => self.model_catalog_confirm(),
                // REQ-006：命令面板 Enter——执行选中候选。
                Mode::CommandPalette => self.command_palette_confirm(),
                // APPROVAL 仅 y/n/q/Esc/a（§3 键位边界；Enter 无语义，no-op）。
                Mode::Normal if self.outline.open => self.outline_confirm(),
                _ => vec![],
            },
            C::ApprovalAllow => self.approval_decide(ApprovalOutcome::AllowedOnce),
            C::ApprovalReject => self.approval_decide(ApprovalOutcome::Rejected),
            C::ApprovalCancel => self.approval_decide(ApprovalOutcome::Cancelled),
            C::ApprovalAlways => {
                if self.approval.queue.head_requires_ack() {
                    // danger-full-access 项：`a` = 风险确认（第二层确认，
                    // AC-006-16；确认后仍仅 allowed-once，不提升策略 D-037）。
                    if self.approval.queue.ack_active() {
                        self.approval.acked = true;
                        self.approval.toast =
                            Some("风险已确认；仍仅授权本次（allowed-once）".to_string());
                    }
                    vec![]
                } else {
                    // `a` 非 outcome 词表：TUI 不代远端切换 approval/policy
                    // =never（REQ-I04 V0.4），只显示指引（D-18）。
                    self.approval.toast = Some(
                        "始终允许需在官方 web 策略设置中切换（approval/policy=never，V0.4）"
                            .to_string(),
                    );
                    vec![]
                }
            }
            // REQ-006 审批列表/批量/重试（D-036）。
            C::OpenApprovalList => self.approval_open_list(),
            C::ApprovalRetry => self.approval_retry_list_item(),
            C::ApprovalBatchAllow => self.approval_batch_allow(),
            C::CloseApprovalList => self.approval_close_list(),
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
                if self.mode == Mode::Approval && self.approval.list_open {
                    // APPROVAL 列表中 `q` = 回单条槽不中止（REQ-006 D-036）。
                    self.approval_close_list()
                } else if self.mode == Mode::Approval {
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
            // REQ-006：模型目录 `M` 打开（仅 NORMAL；打开即拉取目录）。
            C::OpenModelCatalog => {
                if self.mode == Mode::Normal {
                    self.open_model_catalog()
                } else {
                    vec![]
                }
            }
            // REQ-006：命令面板 `:` 打开（仅 NORMAL）。
            C::OpenCommandPalette => {
                if self.mode == Mode::Normal {
                    self.open_command_palette()
                } else {
                    vec![]
                }
            }
            // REQ-006：INSERT Tab 呼出命令面板（预填当前 `/` 斜杠命令词，
            // FR-006-03 基础补全；Esc 回 composer 保留草稿）。
            C::ComposerTabComplete => {
                if self.mode != Mode::Insert || !self.composer.visible {
                    return vec![];
                }
                let prefix = self.draft_slash_command_prefix();
                if prefix.is_empty() {
                    self.notice = Some("Tab 补全：先输入 / 开头的斜杠命令名".to_string());
                    return vec![];
                }
                self.open_command_palette_with_query(prefix)
            }
            // REQ-006：`gv` 循环侧栏视图（本地态，AC-006-03/11 无写）。
            C::CycleSidebarView => {
                if self.mode == Mode::Normal {
                    self.sidebar_view.cycle();
                    self.clamp_sidebar_cursor();
                    self.notice = Some(format!(
                        "视图: group={} order={}",
                        self.sidebar_view.group_by.as_str(),
                        self.sidebar_view.order_by.as_str()
                    ));
                }
                vec![]
            }

            // REQ-005：Chat（NORMAL）`gt`/`2` → Trajectory（AC-005-01）。
            C::ToggleTrajectory => {
                if self.mode == Mode::Normal {
                    self.switch_to_trajectory();
                }
                vec![]
            }
            // `gT`/`1` 在 Chat 已是目标 Tab：no-op。
            C::GotoChat => vec![],
            // REQ-005：折叠/详情仅 Trajectory 模式语义（此处穷尽性 arm）。
            C::ToggleFold | C::OpenDetail => vec![],

            // REQ-009 monitor 键位：主界面 Chat 上下文为 no-op（monitor
            // 状态机自行处理，两套状态机共存于同一二进制）。
            C::MonitorOpenChat | C::MonitorStats | C::MonitorCheer | C::MonitorLocate => vec![],
            C::Resize { width, height } => self.handle(AppEvent::Resize { width, height }),
        }
    }

    // ---------- REQ-005: Trajectory 模式机与详情子层（D-25） ----------

    /// 当前活跃会话的轨迹窗口（只读）。
    fn active_traj_window(&self) -> Option<&crate::model::TrajectoryWindow> {
        self.active_session
            .as_ref()
            .and_then(|id| self.traj_sessions.get(&id.0))
    }

    /// Trajectory 模式命令分派。返回 Some = 已处理（模态上下文语义，
    /// D-25：详情子层 q 关面板、列表焦点 q 退出、y 复制、j/k 详情滚动）。
    /// 返回 None = 非 Trajectory 专属命令（交全局 handle_command 现有逻辑，
    /// 如 Resize/CycleFocus 全局语义保持）。
    fn handle_trajectory_command(&mut self, cmd: crate::input::Command) -> Option<Vec<Cmd>> {
        use crate::input::Command as C;
        match cmd {
            C::ToggleTrajectory | C::GotoChat => {
                self.switch_to_chat();
                Some(vec![])
            }
            C::MoveDown | C::MoveUp => {
                if self.traj.detail_open && self.focus == Focus::Details {
                    // 详情子层：j/k 滚动详情文本。
                    let max = self.traj_detail_row_count().saturating_sub(1);
                    match cmd {
                        C::MoveDown => {
                            self.traj.detail_scroll = (self.traj.detail_scroll + 1).min(max)
                        }
                        C::MoveUp => {
                            self.traj.detail_scroll = self.traj.detail_scroll.saturating_sub(1)
                        }
                        _ => {}
                    }
                } else if self.traj.filter.open {
                    // 过滤列表：j/k 移动命中选中（N/M matches 口径）。
                    self.traj_index_rebuild();
                    let n = self.traj_filter_hit_len();
                    if matches!(cmd, C::MoveDown) {
                        self.traj.filter.cursor =
                            (self.traj.filter.cursor + 1).min(n.saturating_sub(1));
                    } else {
                        self.traj.filter.cursor = self.traj.filter.cursor.saturating_sub(1);
                    }
                } else if self.focus == Focus::Center {
                    self.move_traj_cursor(matches!(cmd, C::MoveDown));
                }
                Some(vec![])
            }
            C::ToggleFold => {
                self.toggle_traj_fold();
                Some(vec![])
            }
            C::OpenDetail => {
                self.open_traj_detail();
                Some(vec![])
            }
            C::StartSearch => {
                // 轨迹内过滤：仍在 Trajectory 模式（与 Chat 结构化搜索分离，
                // 不串模式）；本地 nucleo 窗口内即时过滤（AC-005-05）。
                self.traj.filter.open = true;
                self.traj.filter.query.clear();
                self.traj.filter.cursor = 0;
                self.traj_index_rebuild();
                Some(vec![])
            }
            // 过滤输入态（InputMode::TrajectoryFilter）字符/删除/Enter。
            C::PickerInput(text) => {
                if self.traj.filter.open {
                    self.traj.filter.query.push_str(&text);
                    self.traj.filter.cursor = 0;
                }
                Some(vec![])
            }
            C::PickerBackspace => {
                if self.traj.filter.open {
                    self.traj.filter.query.pop();
                    self.traj.filter.cursor = 0;
                }
                Some(vec![])
            }
            C::PickerConfirm => {
                if self.traj.filter.open {
                    self.jump_to_traj_match();
                }
                Some(vec![])
            }
            C::YankContext => {
                if self.traj.detail_open && self.focus == Focus::Details {
                    Some(self.yank_traj_detail())
                } else {
                    Some(vec![])
                }
            }
            C::Quit => {
                if self.traj.detail_open && self.focus == Focus::Details {
                    self.close_traj_detail();
                    Some(vec![])
                } else if self.traj.filter.open {
                    self.close_traj_filter();
                    Some(vec![])
                } else {
                    // 轨迹列表焦点：全局 q（运行中先 stop 确认）。
                    Some(self.quit())
                }
            }
            C::ClosePicker => {
                if self.traj.detail_open && self.focus == Focus::Details {
                    self.close_traj_detail();
                } else if self.traj.filter.open {
                    self.close_traj_filter();
                }
                Some(vec![])
            }
            C::CycleFocus => {
                // Ctrl+w：Sidebar ↔ Center(Trajectory) ↔ Details（详情开时）。
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Center,
                    Focus::Center if self.traj.detail_open => Focus::Details,
                    Focus::Details => Focus::Sidebar,
                    Focus::Center => Focus::Sidebar,
                };
                Some(vec![])
            }
            C::OpenSelected
            | C::OpenFocused
            | C::OpenPicker
            | C::InsertMode
            | C::OpenOutline
            | C::OpenHelp
            | C::StopRunning
            | C::CollapseProject
            | C::ExpandProject
            | C::VisualStart { .. }
            | C::RetryProbe => Some(vec![]),
            C::GotoTop => {
                // S2 修复：轨迹 gg 跳列表顶 + 触发**轨迹自己的**历史分页
                // （窗口 head_has_more + Ready + single-flight），不再落全局
                // scroll 改隐藏 Chat 视口（AC-005-07 独立 loadOlder seam）。
                let before = self.traj.cursor;
                self.traj.cursor = 0;
                if self.traj.detail_open && self.traj.cursor != before {
                    self.rebuild_traj_detail();
                }
                let has_more = self
                    .active_traj_window()
                    .map(|w| w.head_has_more())
                    .unwrap_or(false);
                let mut cmds = vec![];
                if has_more && self.conn == ConnState::Ready && !self.page_guard.in_flight {
                    cmds.push(self.page_cmd());
                }
                Some(cmds)
            }
            C::GotoBottom => {
                // 轨迹 G 跳列表底（最新事件），对齐 Chat follow_tail 语义。
                let len = self.traj_view_len();
                let before = self.traj.cursor;
                self.traj.cursor = len.saturating_sub(1);
                if self.traj.detail_open && self.traj.cursor != before {
                    self.rebuild_traj_detail();
                }
                Some(vec![])
            }
            // 其余（Resize 等）交全局逻辑。
            _ => None,
        }
    }

    /// 详情文本行数（供 detail_scroll clamp；由 Detail 行布局决定，UI 层
    /// 同步裁剪）。
    fn traj_detail_row_count(&self) -> usize {
        let mut n = 0usize;
        if let Some(d) = &self.traj.detail {
            n += 4; // title + 分隔线基础行
            if let Some(args) = &d.args_text {
                n += args.lines().count() + 1;
            }
            if let Some(result) = &d.result_text {
                n += result.lines().count() + 1;
            }
            n += 4; // error / usage / timing / diff 基础行
            if let Some(diff) = &d.diff {
                n += diff.lines().count() + 1;
            }
        }
        n
    }

    /// 轨迹视图行数（折叠后可见行；AppState 视角供 cursor 移动 clamp）。
    fn traj_view_len(&self) -> usize {
        self.active_traj_window()
            .map(|w| w.view(&self.traj.fold).len())
            .unwrap_or(0)
    }

    fn move_traj_cursor(&mut self, down: bool) {
        let len = self.traj_view_len();
        let before = self.traj.cursor;
        if down {
            self.traj.cursor = (self.traj.cursor + 1).min(len.saturating_sub(1));
        } else {
            self.traj.cursor = self.traj.cursor.saturating_sub(1);
        }
        // REQ-005 §5「选行变化即重建，source 锚点防串」：详情开着且选中行
        // 变化（focus 切回 Center 后移动光标）→ 详情随新行重建刷新（source
        // seq/kind 同步防串错）。
        if self.traj.detail_open && self.traj.cursor != before {
            self.rebuild_traj_detail();
        }
    }

    /// 按当前选中行重建详情（若行可详查）。
    fn rebuild_traj_detail(&mut self) {
        let detail = {
            let Some(window) = self.active_traj_window() else {
                return;
            };
            let view = window.view(&self.traj.fold);
            let Some(row) = view.get(self.traj.cursor) else {
                return;
            };
            detail_for(row, window)
        };
        self.traj.detail = detail.filter(|_| self.traj.detail_open);
        if self.traj.detail.is_none() {
            // 新行不可详查：关闭详情子层回列表。
            self.traj.detail_open = false;
            self.traj.detail_scroll = 0;
            self.focus = Focus::Center;
        } else {
            self.traj.detail_scroll = 0;
        }
    }

    fn toggle_traj_fold(&mut self) {
        // 取选中行的折叠组（借用分离：先只读 RowId → 再改 fold）。
        let group = {
            let Some(window) = self.active_traj_window() else {
                return;
            };
            let view = window.view(&self.traj.fold);
            let Some(row) = view.get(self.traj.cursor) else {
                return;
            };
            window.group_of(row.id())
        };
        if let Some(g) = group {
            let now_collapsed = self.traj.fold.toggle(g);
            // 折叠后 cursor 定位到该组组首（折叠后唯一保留的组成员，仍可经
            // group_of 识别）——再次 za 时 cursor 落在组首，toggle 能命中同
            // 组展开（AC-005-04/12 z/za 往返正确）。
            if now_collapsed {
                if let Some(window) = self.active_traj_window() {
                    let view = window.view(&self.traj.fold);
                    if let Some(pos) = view.iter().position(|r| window.group_of(r.id()) == Some(g))
                    {
                        self.traj.cursor = pos;
                    }
                }
            }
            self.traj.cursor = self.traj.cursor.min(self.traj_view_len().saturating_sub(1));
        }
    }

    fn open_traj_detail(&mut self) {
        // 计算详情（纯函数 detail_for），计算结束即释放窗口借用。
        let detail = {
            let Some(window) = self.active_traj_window() else {
                return;
            };
            let view = window.view(&self.traj.fold);
            let Some(row) = view.get(self.traj.cursor) else {
                return;
            };
            detail_for(row, window)
        };
        if let Some(d) = detail {
            self.traj.detail = Some(d);
            self.traj.detail_open = true;
            self.traj.detail_scroll = 0;
            self.focus = Focus::Details;
        }
    }

    fn close_traj_detail(&mut self) {
        self.traj.detail_open = false;
        self.traj.detail = None;
        self.traj.detail_scroll = 0;
        self.focus = Focus::Center;
    }

    /// y 复制详情 args/result 纯文本（AC-005-11；走 Cmd::CopyToClipboard，
    /// main 里 arboard→OSC52→tmux 执行，不落盘）。
    fn yank_traj_detail(&mut self) -> Vec<Cmd> {
        let Some(detail) = &self.traj.detail else {
            return vec![];
        };
        match detail.yank_text() {
            Some(text) => {
                self.notice = Some("copied".to_string());
                vec![Cmd::CopyToClipboard { text }]
            }
            None => vec![],
        }
    }

    /// 从 Trajectory 切回 Chat（先关详情子层；折叠/选中随 traj 状态保留，
    /// AC-005-01）。
    fn switch_to_chat(&mut self) {
        if self.traj.detail_open {
            self.close_traj_detail();
        }
        if self.traj.filter.open {
            self.traj.filter.open = false;
        }
        self.mode = Mode::Normal;
        self.focus = Focus::Center;
    }

    /// 从 Chat 切到 Trajectory（gt；无会话也能切，空轨迹视图展示）。
    pub fn switch_to_trajectory(&mut self) {
        self.mode = Mode::Trajectory;
        self.focus = Focus::Center;
        // 活跃会话的轨迹窗口确保存在（触达：首帧渲染空、事件到达后填充）。
        if let Some(id) = self.active_session.clone() {
            let _ = self.traj_sessions.touch(&id.0, self.window_cap);
        }
        self.traj.cursor = self.traj.cursor.min(self.traj_view_len().saturating_sub(1));
    }

    // ---------- REQ-005 Step 4：轨迹内搜索（本地 nucleo 过滤） ----------

    /// 重建轨迹搜索索引（过滤打开/输入/窗口变化后；≤200 行，成本可忽略）。
    fn traj_index_rebuild(&mut self) {
        // 借用分离：先在只读 self 上构建新索引，再整体赋值（避免
        // active_traj_window 与 traj_search_index 可变借用冲突）。
        let rebuilt = {
            let mut index = crate::model::TrajectorySearchIndex::new();
            if let Some(window) = self.active_traj_window() {
                index.rebuild(window.raw_rows());
            }
            index
        };
        self.traj_search_index = rebuilt;
    }

    /// 当前过滤词命中数（N/M matches 与 cursor clamp 依据）。
    fn traj_filter_hit_len(&self) -> usize {
        self.traj_search_index.query(&self.traj.filter.query).len()
    }

    /// Enter 跳转当前选中命中行：展开其所在折叠组（AC-005-13）→ cursor 定位
    /// → 关闭过滤回完整列表。
    fn jump_to_traj_match(&mut self) {
        let hits = self.traj_search_index.query(&self.traj.filter.query);
        let Some(hit) = hits.get(self.traj.filter.cursor) else {
            // 无命中（cursor 越界/空查询）：直接退出过滤，不跳转。
            self.close_traj_filter();
            return;
        };
        let Some(item) = self.traj_search_index.items().get(hit.item_index).cloned() else {
            return;
        };
        // 展开命中行所在折叠组（若折叠）——跳转后行必须可见。
        if let Some(group) = self
            .active_traj_window()
            .and_then(|w| w.group_of(item.row_id))
        {
            self.traj.fold.expand(group);
        }
        // cursor 定位到命中行（完整折叠视图内）。
        if let Some(pos) = self
            .active_traj_window()
            .map(|w| w.view(&self.traj.fold))
            .and_then(|view| view.iter().position(|r| r.id() == item.row_id))
        {
            self.traj.cursor = pos;
        }
        self.close_traj_filter();
    }

    fn close_traj_filter(&mut self) {
        self.traj.filter.open = false;
        self.traj.filter.query.clear();
        self.traj.filter.cursor = 0;
        self.focus = Focus::Center;
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
        // REQ-007 AC-007-31：非空 query 记入搜索历史（模型去重 + FIFO 50）。
        if !self.search.query.trim().is_empty() {
            self.query_history.push(self.search.query.trim());
        }
        self.search.recall_cursor = None;
        self.mode = Mode::Normal;
        self.search.open = false;
        self.search.history_generation = self.search.history_generation.wrapping_add(1);
        self.search.history_loading = false;
        self.search.results_locked = false;
    }

    /// AC-007-31：↑ 回忆更早的最近查询（仅空 query 编辑态）。
    fn search_recall_older(&mut self) -> Vec<Cmd> {
        let rec: Vec<String> = self.query_history.recent().map(String::from).collect();
        if rec.is_empty() {
            return vec![];
        }
        // 起始或顶部：取最近一条；已在游标：往更早走。
        let cur = self.search.recall_cursor;
        let next = cur.map(|c| c + 1).unwrap_or(0);
        if next >= rec.len() {
            return vec![]; // 已到最旧（顶部停留）
        }
        self.search.recall_cursor = Some(next);
        self.search.query = rec[next].clone();
        self.recompute_window_matches();
        self.search.history_error = None;
        vec![]
    }

    /// AC-007-31：↓ 回到更新的查询（越过最新则清空回手动输入）。
    fn search_recall_newer(&mut self) -> Vec<Cmd> {
        let Some(cur) = self.search.recall_cursor else {
            return vec![];
        };
        if cur == 0 {
            self.search.recall_cursor = None;
            self.search.query.clear();
            self.search.window_matches.clear();
            return vec![];
        }
        let rec: Vec<String> = self.query_history.recent().map(String::from).collect();
        if let Some(q) = rec.get(cur - 1) {
            self.search.recall_cursor = Some(cur - 1);
            self.search.query = q.clone();
            self.recompute_window_matches();
        }
        vec![]
    }

    // ---------- REQ-006 Sidebar 行光标与视图态（FR-006-02，D-034） ----------

    /// 侧栏可见行数（视图态行序，`sidebar_rows` 纯函数；UI 与 reducer 共享）。
    fn sidebar_row_len(&self) -> usize {
        crate::model::sidebar_rows(&self.sidebar_view, &self.workspaces).len()
    }

    fn clamp_sidebar_cursor(&mut self) {
        let len = self.sidebar_row_len();
        if len == 0 {
            self.sidebar.cursor = 0;
        } else if self.sidebar.cursor >= len {
            self.sidebar.cursor = len - 1;
        }
    }

    /// j/k（Focus::Sidebar）：移动行光标（不触发 Chat 滚动）。
    fn sidebar_cursor_move(&mut self, down: bool) {
        let len = self.sidebar_row_len();
        if len == 0 {
            return;
        }
        if down {
            self.sidebar.cursor = (self.sidebar.cursor + 1).min(len - 1);
        } else {
            self.sidebar.cursor = self.sidebar.cursor.saturating_sub(1);
        }
    }

    /// 光标行目标（session / workspace header）。
    fn sidebar_cursor_row(&self) -> Option<crate::model::SidebarRow> {
        crate::model::sidebar_rows(&self.sidebar_view, &self.workspaces)
            .into_iter()
            .nth(self.sidebar.cursor)
    }

    /// Enter/`o`（Focus::Sidebar）：打开光标行——session → open follow；
    /// workspace header → 折叠/展开（本地视图态）。
    fn open_sidebar_cursor_row(&mut self) -> Vec<Cmd> {
        use crate::model::SidebarRow as Row;
        match self.sidebar_cursor_row() {
            Some(Row::Session(id)) => self.open_session(id),
            Some(Row::WorkspaceHeader { id, .. }) => {
                self.sidebar_view.toggle(&id);
                self.clamp_sidebar_cursor();
                vec![]
            }
            None => vec![],
        }
    }

    // ---------- REQ-006 模型目录（FR-006-01） ----------

    /// `M` 打开模型目录 overlay：重置为加载态并拉取 `session/modelCatalog`
    /// （单飞 generation；仅 NORMAL 语义，调用方已检查）。
    fn open_model_catalog(&mut self) -> Vec<Cmd> {
        self.mode = Mode::ModelCatalog;
        self.model_catalog.reset_for_open();
        // 读一次当前官方 modelSelection 投影（ADR-008；会话级镜像）。
        let projections = self
            .active_window()
            .map(|w| ProjectionSnapshot::new(w.projections().clone()));
        if let Some(p) = projections.as_ref() {
            let sel = p.model_selection();
            self.model_catalog.current_model = sel.last_used;
            self.model_catalog.next_model = sel.next;
        }
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        vec![Cmd::FetchModelCatalog {
            generation: self.catalog_generation,
        }]
    }

    /// q/Esc 关闭模型目录（effort 子阶段已先退一级；此处直接关闭）。
    fn close_model_catalog(&mut self) {
        self.model_catalog.visible = false;
        self.model_catalog.effort = None;
        self.model_catalog.selecting = None;
        // 作废在途目录拉取（迟到响应丢弃）。
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        self.mode = Mode::Normal;
    }

    /// 主列表过滤命中（UI 与 reducer 共用同一 seam：`CatalogIndex::query`）。
    fn model_catalog_hits(&self) -> Vec<&crate::model::ModelCatalogItem> {
        self.model_catalog.index.query(&self.model_catalog.query)
    }

    /// j/k 移动光标：effort 子阶段移 effort 候选；主列表移命中行。
    fn model_catalog_cursor_move(&mut self, down: bool) {
        if let Some(pick) = self.model_catalog.effort.as_mut() {
            if down {
                if !pick.efforts.is_empty() {
                    pick.cursor = (pick.cursor + 1).min(pick.efforts.len() - 1);
                }
            } else {
                pick.cursor = pick.cursor.saturating_sub(1);
            }
            return;
        }
        let total = self.model_catalog_hits().len();
        if total == 0 {
            return;
        }
        if down {
            self.model_catalog.selection = (self.model_catalog.selection + 1).min(total - 1);
        } else {
            self.model_catalog.selection = self.model_catalog.selection.saturating_sub(1);
        }
    }

    /// 字符输入进 query（effort 子阶段输入忽略，避免误改查询词）。
    fn model_catalog_input(&mut self, text: &str) {
        if self.model_catalog.effort.is_some() {
            return;
        }
        self.model_catalog.query.push_str(text);
        self.model_catalog.selection = 0;
    }

    fn model_catalog_backspace(&mut self) {
        if self.model_catalog.effort.is_some() {
            return;
        }
        self.model_catalog.query.pop();
        self.model_catalog.selection = 0;
    }

    /// Enter：主列表 → 选中模型（自带 efforts → 进 effort 子阶段，否则直接
    /// 热切换）；effort 子阶段 → 确认 effort 提交。Error 态 Enter = 重试加载。
    /// selectModel 单飞：`selecting` 在途时忽略（同一弹窗只提交一次，
    /// AC-006-15 同源守卫）。
    fn model_catalog_confirm(&mut self) -> Vec<Cmd> {
        if self.model_catalog.selecting.is_some() {
            return vec![];
        }
        match self.model_catalog.phase {
            CatalogPhase::Error => {
                // 重试加载（恢复后重试成功，AC-006-06）。
                self.model_catalog.phase = CatalogPhase::Loading;
                self.model_catalog.load_error = None;
                self.model_catalog.last_error_code = None;
                self.catalog_generation = self.catalog_generation.wrapping_add(1);
                return vec![Cmd::FetchModelCatalog {
                    generation: self.catalog_generation,
                }];
            }
            CatalogPhase::Loading => return vec![],
            CatalogPhase::Ready => {}
        }
        // effort 子阶段确认。
        if let Some(pick) = self.model_catalog.effort.clone() {
            let effort = pick
                .efforts
                .get(pick.cursor)
                .cloned()
                .or(pick.default.clone());
            self.model_catalog.effort = None;
            self.model_catalog.selection = 0;
            return self.model_catalog_submit(pick.provider_id, pick.model_id, effort);
        }
        // 主列表：选中行（与 UI 同 query 同序）。
        let Some(item) = self
            .model_catalog_hits()
            .into_iter()
            .nth(self.model_catalog.selection)
            .cloned()
        else {
            return vec![];
        };
        if !item.reasoning_efforts.is_empty() {
            // 模型自带 reasoning efforts → 先选 effort（不越界 V0.4）。
            let default_idx = item
                .default_effort
                .as_ref()
                .and_then(|d| item.reasoning_efforts.iter().position(|e| e == d))
                .unwrap_or(0);
            self.model_catalog.effort = Some(EffortPick {
                provider_id: item.provider_id,
                model_id: item.model_id,
                model_name: item.model_name,
                efforts: item.reasoning_efforts,
                default: item.default_effort,
                cursor: default_idx,
            });
            return vec![];
        }
        self.model_catalog_submit(item.provider_id, item.model_id, None)
    }

    /// 发起 `session/selectModel`（无活动会话 → 目录区提示，不发命令）。
    fn model_catalog_submit(
        &mut self,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Vec<Cmd> {
        if self.active_session.is_none() {
            self.model_catalog.load_error =
                Some("无打开的会话：先用 f/o 打开会话再切换模型".into());
            return vec![];
        }
        self.model_catalog.selecting = Some((provider.clone(), model.clone()));
        self.model_catalog.load_error = None;
        self.model_catalog.last_error_code = None;
        vec![Cmd::SelectModel {
            provider,
            model,
            reasoning_effort,
        }]
    }

    // ---------- REQ-006 命令面板（FR-006-03，`:`） ----------

    /// `:` 打开命令面板：重置输入态；有活动会话则拉取远端斜杠命令表
    /// （fetch-once：已拉取重开复用不重拉）。
    fn open_command_palette(&mut self) -> Vec<Cmd> {
        self.open_command_palette_with_query(String::new())
    }

    /// 打开命令面板并预填 query（composer Tab 补全 / `:` 均走此；前缀以
    /// `/` 开头时命中远端斜杠命令候选）。
    fn open_command_palette_with_query(&mut self, query: String) -> Vec<Cmd> {
        self.mode = Mode::CommandPalette;
        self.command_palette.reset_for_open();
        self.command_palette.query = query;
        let mut cmds = Vec::new();
        if !self.command_palette.remote_fetched && self.active_session.is_some() {
            cmds.push(Cmd::FetchRemoteCommands);
        }
        cmds
    }

    /// q/Esc 关闭命令面板：从 composer Tab 进入时回 INSERT（草稿保留，
    /// 下次 Enter 仍是发送 prompt）；否则回 NORMAL。
    fn close_command_palette(&mut self) {
        self.command_palette.visible = false;
        self.command_palette.executing = false;
        self.command_palette.stage = None;
        if self.composer.visible {
            self.mode = Mode::Insert;
        } else {
            self.mode = Mode::Normal;
        }
    }

    /// j/k 移动候选光标。
    fn command_palette_cursor_move(&mut self, down: bool) {
        let total = self.command_palette.filtered().len();
        if total == 0 {
            return;
        }
        if down {
            self.command_palette.selection = (self.command_palette.selection + 1).min(total - 1);
        } else {
            self.command_palette.selection = self.command_palette.selection.saturating_sub(1);
        }
    }

    /// 字符输入（输入过滤；重新选中首项）。
    fn command_palette_input(&mut self, text: &str) {
        self.command_palette.query.push_str(text);
        self.command_palette.selection = 0;
        self.command_palette.last_result = None;
        self.command_palette.last_error_code = None;
    }

    fn command_palette_backspace(&mut self) {
        self.command_palette.query.pop();
        self.command_palette.selection = 0;
        self.command_palette.last_result = None;
        self.command_palette.last_error_code = None;
    }

    /// Enter：执行选中候选（本地动作 → reducer 直执行；远端 → Cmd 单飞；
    /// V0.4 → 面板提示不执行）。
    fn command_palette_confirm(&mut self) -> Vec<Cmd> {
        // 操作子阶段优先：参数输入提交 / 破坏性确认执行。
        if let Some(stage) = self.command_palette.stage.clone() {
            return match stage {
                PaletteStage::Input { kind, .. } => self.palette_stage_input_commit(kind),
                PaletteStage::ConfirmDanger { op, .. } => self.palette_send_op(op),
            };
        }
        let Some(item) = self
            .command_palette
            .filtered()
            .into_iter()
            .nth(self.command_palette.selection)
        else {
            return vec![];
        };
        match item {
            CommandPaletteItem::Local { action, .. } => match action {
                // workspace/session 操作：留在面板（输入/确认子阶段或直接发）。
                PaletteAction::ForkSession
                | PaletteAction::RenameSession
                | PaletteAction::ArchiveSession
                | PaletteAction::MoveSession
                | PaletteAction::NewWorkspace
                | PaletteAction::RenameWorkspace
                | PaletteAction::DeleteWorkspace => self.palette_begin_operation(action),
                action => {
                    self.command_palette.visible = false;
                    // 从 composer Tab 进入时 Local 动作后回 INSERT（草稿保留）。
                    self.mode = if self.composer.visible {
                        Mode::Insert
                    } else {
                        Mode::Normal
                    };
                    match action {
                        PaletteAction::ModelCatalog => self.open_model_catalog(),
                        PaletteAction::CycleSidebarView => {
                            self.sidebar_view.cycle();
                            self.clamp_sidebar_cursor();
                            self.notice = Some(format!(
                                "视图: group={} order={}",
                                self.sidebar_view.group_by.as_str(),
                                self.sidebar_view.order_by.as_str()
                            ));
                            vec![]
                        }
                        PaletteAction::CollapseAll => {
                            self.sidebar_view.collapse_all(&self.workspaces);
                            self.clamp_sidebar_cursor();
                            vec![]
                        }
                        PaletteAction::ExpandAll => {
                            self.sidebar_view.expand_all();
                            self.clamp_sidebar_cursor();
                            vec![]
                        }
                        PaletteAction::NewSession => {
                            // 关闭面板（保持 NORMAL）再发 create；结果事件返回。
                            vec![Cmd::CreateSession]
                        }
                        PaletteAction::Help => {
                            self.help_open = true;
                            vec![]
                        }
                        PaletteAction::ToggleTheme => {
                            // 立即重绘（palette 重建），并持久化 config.toml。
                            self.toggle_theme();
                            vec![Cmd::SaveUiTheme {
                                theme: self.palette.theme.clone(),
                                palette: self.palette_overrides.clone(),
                            }]
                        }
                        PaletteAction::EditWithEditor => {
                            // 面板已关闭；返回 :edit 起始 Cmd（若在 INSERT）。
                            self.external_edit_begin()
                        }
                        PaletteAction::OpenSubagents => self.open_subagents(),
                        PaletteAction::OpenGoal => self.open_goal_panel(),
                        PaletteAction::OpenJobs => self.open_jobs_panel(),
                        PaletteAction::OpenSettings => self.open_settings_panel(),
                        PaletteAction::OpenSkills => self.open_skills_panel(),
                        PaletteAction::OpenExport => self.open_export_panel(),
                        PaletteAction::ForkSession
                        | PaletteAction::RenameSession
                        | PaletteAction::ArchiveSession
                        | PaletteAction::MoveSession
                        | PaletteAction::NewWorkspace
                        | PaletteAction::RenameWorkspace
                        | PaletteAction::DeleteWorkspace => unreachable!("上方已处理"),
                    }
                }
            },
            CommandPaletteItem::Remote { name, .. } => {
                // 单飞：同一命令行只提交一次（AC-006-12/13）。
                if self.command_palette.executing {
                    return vec![];
                }
                // 执行完整斜杠行（query 中可能带参数，如 `/plan off`）。
                let line = if self.command_palette.query.trim_start().starts_with('/') {
                    self.command_palette.query.trim().to_string()
                } else {
                    format!("/{name}")
                };
                self.command_palette.executing = true;
                self.command_palette.last_result = None;
                self.command_palette.last_error_code = None;
                vec![Cmd::ExecuteCommand { line }]
            }
            CommandPaletteItem::V04 { label, .. } => {
                // 非目标项：显示禁用提示（REQ-007 V0.4），面板保持可输入。
                self.command_palette.last_result =
                    Some(format!("{label} 在 V0.4 提供（当前版本不可用）"));
                vec![]
            }
        }
    }

    /// 操作项开始：目标校验 → 无参操作直接发（返回其 Cmd）/ 需参数进入输入
    /// 子阶段（返回空）/ 危险操作先确认。
    fn palette_begin_operation(&mut self, action: PaletteAction) -> Vec<Cmd> {
        // 校验目标是否存在（会话操作 → active_session；workspace 操作 →
        // sidebar 光标 workspace）。
        match action {
            PaletteAction::ForkSession
            | PaletteAction::RenameSession
            | PaletteAction::ArchiveSession
            | PaletteAction::MoveSession => {
                if self.active_session.is_none() {
                    self.command_palette.last_result = Some("无活动会话：先用 f/o 打开会话".into());
                    return vec![];
                }
            }
            PaletteAction::NewWorkspace => {}
            PaletteAction::RenameWorkspace | PaletteAction::DeleteWorkspace => {
                if self.palette_workspace_target().is_none() {
                    self.command_palette.last_result =
                        Some("请先将侧栏光标移到 workspace 或其会话".into());
                    return vec![];
                }
            }
            _ => unreachable!(),
        }
        match action {
            PaletteAction::ForkSession => {
                // 无参直接发（fork 当前活动会话）。
                let op = WorkspaceOperation::ForkSession {
                    session_id: self.active_session.clone().unwrap(),
                };
                self.palette_execute_op(op)
            }
            PaletteAction::ArchiveSession => {
                let op = WorkspaceOperation::ArchiveSession {
                    session_id: self.active_session.clone().unwrap(),
                };
                // dangerous → 进入确认（返回空）。
                self.palette_execute_op(op)
            }
            PaletteAction::DeleteWorkspace => {
                let wid = self.palette_workspace_target().unwrap();
                let op = WorkspaceOperation::DeleteWorkspace { workspace_id: wid };
                // dangerous → 进入确认（返回空）。
                self.palette_execute_op(op)
            }
            PaletteAction::RenameSession => {
                self.command_palette
                    .begin_input(OpArgKind::RenameSession, "会话新标题");
                vec![]
            }
            PaletteAction::MoveSession => {
                self.command_palette
                    .begin_input(OpArgKind::MoveSession, "目标 workspace id");
                vec![]
            }
            PaletteAction::NewWorkspace => {
                self.command_palette
                    .begin_input(OpArgKind::NewWorkspace, "workspace 路径");
                vec![]
            }
            PaletteAction::RenameWorkspace => {
                self.command_palette
                    .begin_input(OpArgKind::RenameWorkspace, "workspace 新标题");
                vec![]
            }
            _ => unreachable!(),
        }
    }

    /// 参数输入子阶段 Enter：按类型构建操作并执行（危险操作先进确认）。
    fn palette_stage_input_commit(&mut self, kind: OpArgKind) -> Vec<Cmd> {
        let text = self.command_palette.query.trim().to_string();
        let op = match kind {
            OpArgKind::RenameSession => {
                let Some(session_id) = self.active_session.clone() else {
                    self.command_palette.last_result = Some("无活动会话：先用 f/o 打开会话".into());
                    return vec![];
                };
                if text.is_empty() {
                    self.command_palette.last_result = Some("标题不能为空".into());
                    return vec![];
                }
                Some(WorkspaceOperation::RenameSession {
                    session_id,
                    title: text,
                })
            }
            OpArgKind::NewWorkspace => {
                if text.is_empty() {
                    self.command_palette.last_result = Some("路径不能为空".into());
                    return vec![];
                }
                Some(WorkspaceOperation::NewWorkspace { path: text })
            }
            OpArgKind::RenameWorkspace => {
                if text.is_empty() {
                    self.command_palette.last_result = Some("标题不能为空".into());
                    return vec![];
                }
                let Some(workspace_id) = self.palette_workspace_target() else {
                    self.command_palette.last_result =
                        Some("请先将侧栏光标移到 workspace 或其会话".into());
                    return vec![];
                };
                Some(WorkspaceOperation::RenameWorkspace {
                    workspace_id,
                    title: text,
                })
            }
            OpArgKind::MoveSession => {
                let Some(session_id) = self.active_session.clone() else {
                    self.command_palette.last_result = Some("无活动会话：先用 f/o 打开会话".into());
                    return vec![];
                };
                // wire 无「移出分组」端点：目标 workspace 必填（可空提示已改）。
                if text.is_empty() {
                    self.command_palette.last_result = Some("目标 workspace id 不能为空".into());
                    return vec![];
                }
                Some(WorkspaceOperation::MoveSession {
                    session_id,
                    target_workspace: crate::api::types::WorkspaceId(text),
                })
            }
        };
        let Some(op) = op else {
            return vec![];
        };
        self.palette_execute_op(op)
    }

    /// 目标 workspace = sidebar 光标行（header 或会话所属）。
    fn palette_workspace_target(&self) -> Option<crate::api::types::WorkspaceId> {
        use crate::model::SidebarRow as Row;
        match self.sidebar_cursor_row()? {
            Row::WorkspaceHeader { id, .. } => Some(id),
            Row::Session(id) => self
                .workspaces
                .sessions
                .get(&id)
                .and_then(|m| m.workspace.clone()),
        }
    }

    /// 执行写操作：危险 → 二次确认阶段；否则直接发（requestId 单飞）。
    fn palette_execute_op(&mut self, op: WorkspaceOperation) -> Vec<Cmd> {
        if self.command_palette.op_inflight.is_some() {
            self.command_palette.last_result = Some("有操作在途，请等待完成后再试".into());
            return vec![];
        }
        if op.dangerous() {
            self.command_palette.begin_confirm(op);
            return vec![];
        }
        self.palette_send_op(op)
    }

    /// 真正发送写操作 Cmd（requestId 幂等单飞，AC-006-12）。
    fn palette_send_op(&mut self, op: WorkspaceOperation) -> Vec<Cmd> {
        let request_id = crate::api::types::mint_request_id();
        let label = op.label();
        self.command_palette.op_inflight = Some((request_id.clone(), label));
        self.command_palette.stage = None;
        self.command_palette.last_result = None;
        self.command_palette.last_error_code = None;
        vec![Cmd::WorkspaceOp { request_id, op }]
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

    /// 审批决策（y/n/q 共用；同一弹窗只回复一次 — 幂等，AC-003-17/AC-006-15）。
    /// danger 未确认时 AllowedOnce 被拦截（AC-006-16 第二层确认停点）。
    fn approval_decide(&mut self, outcome: ApprovalOutcome) -> Vec<Cmd> {
        if self.mode != Mode::Approval || self.approval.reply_inflight {
            return vec![];
        }
        if outcome == ApprovalOutcome::AllowedOnce && self.approval.queue.head_requires_ack() {
            self.approval.toast =
                Some("危险操作需先确认风险（按 a）后再允许，仍仅授权本次".to_string());
            return vec![];
        }
        let Some(event) = self.approval.queue.active_event() else {
            return vec![];
        };
        self.approval.reply_inflight = true;
        vec![Cmd::ReplyApproval { event, outcome }]
    }

    /// Promote 队列首项到单条展示槽（镜像 `ApprovalState.event`），进入
    /// Approval 模态。prev_mode 仅在非 Approval→Approval 首提时快照一次
    /// （批量自动推进不重快照）。
    fn approval_promote_display(&mut self) {
        let Some(event) = self.approval.queue.promote() else {
            return;
        };
        if self.mode != Mode::Approval {
            self.approval.prev_mode = self.mode;
        }
        self.approval.event = Some(event);
        self.approval.acked = self.approval.queue.head_acked();
        self.approval.visible = true;
        self.approval.reply_inflight = false;
        self.approval.waiting_hint = false;
        self.approval.list_open = false;
        self.mode = Mode::Approval;
    }

    /// 把 queue active 状态镜像回 `ApprovalState`（event/acked/list）。
    fn approval_sync_from_queue(&mut self) {
        self.approval.event = self.approval.queue.active_event();
        self.approval.acked = self.approval.queue.head_acked();
    }

    /// 批量续发：batch_allow 且无 danger 停点且无在途 → 对当前 active 发起
    /// allowed-once（逐条 unary 串行泵；AC-006-15）。danger 停点自动关闭
    /// batch（等 ack，AC-006-16）。
    fn approval_pump_if_batch(&mut self) -> Vec<Cmd> {
        if !self.approval.batch_allow || self.approval.reply_inflight {
            return vec![];
        }
        if self.approval.queue.head_requires_ack() {
            self.approval.batch_allow = false;
            self.approval.toast =
                Some("危险操作需先确认风险（按 a）后继续批量；仅授权本次".to_string());
            return vec![];
        }
        let Some(event) = self.approval.event.clone() else {
            return vec![];
        };
        self.approval.reply_inflight = true;
        vec![Cmd::ReplyApproval {
            event,
            outcome: ApprovalOutcome::AllowedOnce,
        }]
    }

    /// `L`：打开审批列表视图（pending/failed 全量；光标行用于 r 重试）。
    fn approval_open_list(&mut self) -> Vec<Cmd> {
        if self.mode != Mode::Approval {
            return vec![];
        }
        self.approval.list_open = true;
        self.approval.list_cursor = 0;
        vec![]
    }

    /// q/Esc（列表视图）：有单条槽 → 回单条槽不中止（不发出 cancelled）；
    /// 无单条槽（队列只剩失败项）→ 退出 Approval 回先前模式。
    fn approval_close_list(&mut self) -> Vec<Cmd> {
        self.approval.list_open = false;
        self.approval.list_cursor = 0;
        if self.approval.event.is_none() && !self.approval.queue.has_active() {
            // 无单条槽可回：退出（失败项保留在内存，供下次审批/L 重试）。
            self.approval.visible = false;
            self.approval.batch_allow = false;
            self.mode = self.approval.prev_mode;
        }
        vec![]
    }

    /// `r`（列表视图）：重试光标行失败项（回单条槽；若槽空则 promote）。
    fn approval_retry_list_item(&mut self) -> Vec<Cmd> {
        let event_id = {
            let items = self.approval.queue.list();
            items
                .get(self.approval.list_cursor)
                .map(|item| item.event.event_id.clone())
        };
        let Some(event_id) = event_id else {
            return vec![];
        };
        if !self.approval.queue.is_failed(&event_id) {
            return vec![];
        }
        self.approval.queue.retry_failed(&event_id);
        self.approval.list_open = false;
        self.approval.list_cursor = 0;
        self.approval_sync_from_queue();
        if self.approval.event.is_none() {
            self.approval_promote_display();
        }
        vec![]
    }

    /// `A`（列表视图）：批量 allowed-once（串行泵自动续发；danger 停点）。
    fn approval_batch_allow(&mut self) -> Vec<Cmd> {
        if self.mode != Mode::Approval {
            return vec![];
        }
        self.approval.batch_allow = true;
        self.approval.list_open = false;
        // 先对当前 active 发一条（若存在）；后续由 ApprovalReplied 续发。
        self.approval_pump_if_batch()
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

    /// 提取 composer 当前行光标处「/ 开头的斜杠命令词」（Tab 补全预填前缀；
    /// 非 / 开头 → 空）。
    fn draft_slash_command_prefix(&self) -> String {
        let Some(draft) = self.draft.as_ref() else {
            return String::new();
        };
        // 光标前到行首的文本（多行时取当前行，光标未在行首/行中则取
        // 光标前最近一个空白边界；基础版：仅当行首是 / 且光标后无空格）。
        let before_cursor: String = draft.text.chars().take(draft.cursor).collect();
        let current_line = before_cursor.rsplit('\n').next().unwrap_or("");
        let line = current_line.trim_start();
        if !line.starts_with('/') {
            return String::new();
        }
        // 光标前不含空白 → 是可补全词（含 / 本身）。
        if current_line.contains(' ') {
            return String::new();
        }
        current_line.trim().to_string()
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
            self.mark_drafts_dirty();
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
        // REQ-005：会话切换重置轨迹视图（fold/cursor 是会话级；轨迹窗口
        // 触达——打开会话即准备轨迹投影缓存位，事件到达后填充）。
        self.traj.fold = FoldState::default();
        self.traj.cursor = 0;
        self.traj.detail_open = false;
        self.traj.detail = None;
        self.traj.detail_scroll = 0;
        self.traj.filter.open = false;
        let _ = self.traj_sessions.touch(&sid.0, self.window_cap);
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

    // ================= REQ-006 审批队列（D-036） =================

    fn approval_ev_raw(_id: &str, danger: bool) -> serde_json::Value {
        let request = if danger {
            serde_json::json!({"toolName": "danger-full-access", "callId": "c9", "reason": "rm -rf /"})
        } else {
            serde_json::json!({"toolName": "bash", "reason": "ls"})
        };
        serde_json::json!({ "type": "approval/request", "request": request })
    }

    fn approval_ev(id: &str, danger: bool) -> ApprovalEvent {
        ApprovalEvent {
            client_id: format!("c-{id}"),
            event_id: id.to_string(),
            raw: approval_ev_raw(id, danger),
        }
    }

    #[test]
    fn approval_serial_queue_y_y_q_each_once_ac006_15() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        // 三条审批到达：首条 promote 显示，其余入队（不覆盖在途）。
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("e1", false),
        });
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("e2", false),
        });
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("e3", false),
        });
        assert_eq!(s.mode, Mode::Approval);
        assert_eq!(s.approval.queue.summary().pending, 2, "两条排队");
        // y → e1 allowed-once。
        let cmds = s.handle_command(C::ApprovalAllow);
        assert!(
            matches!(&cmds[0], Cmd::ReplyApproval { outcome: ApprovalOutcome::AllowedOnce, event } if event.event_id == "e1")
        );
        // e1 回复成功 → 自动续发显示 e2（≤1 在途）。
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        assert_eq!(
            s.approval.event.as_ref().unwrap().event_id,
            "e2",
            "自动 promote 下一项"
        );
        assert_eq!(s.mode, Mode::Approval, "仍在审批模态");
        // y → e2。
        let cmds = s.handle_command(C::ApprovalAllow);
        assert!(matches!(&cmds[0], Cmd::ReplyApproval { event, .. } if event.event_id == "e2"));
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        // q → e3 cancelled。
        assert_eq!(
            s.approval.event.as_ref().unwrap().event_id,
            "e3",
            "第三条自动展示"
        );
        let cmds = s.handle_command(C::Quit);
        assert!(
            matches!(&cmds[0], Cmd::ReplyApproval { outcome: ApprovalOutcome::Cancelled, event } if event.event_id == "e3")
        );
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::Cancelled,
        });
        // 队列清空 → 回先前模式。
        assert_eq!(s.mode, Mode::Normal);
        assert!(!s.approval.visible);
        assert_eq!(s.approval.queue.summary().pending, 0);
    }

    #[test]
    fn approval_partial_failure_keeps_item_retryable_others_continue_ac006_14() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("e1", false),
        });
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("e2", false),
        });
        s.handle_command(C::ApprovalAllow); // e1 reply in-flight
                                            // e1 回复失败：fail-closed，e1 保留失败项；e2 自动展示可继续。
        s.handle(AppEvent::ApprovalReplyFailed {
            outcome: ApprovalOutcome::AllowedOnce,
            error: ClientError::Transport("eof".into()),
        });
        assert_eq!(
            s.approval.event.as_ref().unwrap().event_id,
            "e2",
            "失败后其余可继续"
        );
        assert!(s.approval.queue.is_failed("e1"), "失败项保留");
        assert!(!s.approval.waiting_hint, "仍有项处理，不收缩等待审批");
        // e2 成功。
        s.handle_command(C::ApprovalAllow);
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        assert_eq!(s.approval.queue.summary().failed, 1, "e1 仍失败");
        assert!(!s.approval.queue.is_empty(), "失败项仍在队列");
        // 打开列表 → r 重试 e1（回单条槽）。
        s.handle_command(C::OpenApprovalList);
        assert!(s.approval.list_open);
        s.handle_command(C::PickerUp); // 光标到顶部（无意义边界测试）
        s.handle_command(C::PickerDown);
        s.handle_command(C::PickerDown); // 光标在 e1(failed) 上（active 空）
        s.handle_command(C::ApprovalRetry);
        assert!(!s.approval.list_open, "重试后回单条槽");
        assert_eq!(s.approval.event.as_ref().unwrap().event_id, "e1");
        assert!(!s.approval.queue.is_failed("e1"));
    }

    #[test]
    fn approval_danger_ack_then_allow_still_allowed_once_ac006_16() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("d1", true),
        });
        // 未 ack 时 y 被拦截（不发 ReplyApproval）。
        assert!(s.handle_command(C::ApprovalAllow).is_empty());
        assert!(!s.approval.reply_inflight);
        // `a` = 风险确认。
        assert!(s.handle_command(C::ApprovalAlways).is_empty());
        assert!(s.approval.acked);
        // ack 后 y → allowed-once（仍不提升策略）。
        let cmds = s.handle_command(C::ApprovalAllow);
        assert!(
            matches!(&cmds[0], Cmd::ReplyApproval { outcome: ApprovalOutcome::AllowedOnce, event } if event.event_id == "d1")
        );
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        assert_eq!(s.mode, Mode::Normal);
        // 已授权事件重放 → 不入队不再授权。
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("d1", true),
        });
        assert_eq!(s.mode, Mode::Normal, "重放已授权事件被拒");
    }

    #[test]
    fn approval_policy_display_read_only_from_projection_ac006_17() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        // projections 带 approval/policy=ask → 只读展示（不弹窗、无切换）。
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(0)),
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({
                "running": false,
                "approvalPolicy": "ask"
            })),
        });
        assert_eq!(s.approval.policy_display, Some("ask"));
        // 帧自身携带 policy（approval/request 到达路径）。
        s.handle(AppEvent::ApprovalRequest {
            event: ApprovalEvent {
                client_id: "c".into(),
                event_id: "e".into(),
                raw: serde_json::json!({
                    "type": "approval/request",
                    "request": {"toolName": "bash", "reason": "x"},
                    "approval/policy": "never"
                }),
            },
        });
        assert_eq!(s.approval.policy_display, Some("never"));
        // 未知/缺失 → None；never 无切换入口（handle_command 无对应命令）。
        s.handle(AppEvent::FollowSnapshot {
            session_id: SessionId("s1".into()),
            cursor: Some(SessionLogOffset(0)),
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({"running": false})),
        });
        assert_eq!(s.approval.policy_display, None);
    }

    #[test]
    fn approval_batch_allow_serial_pump_stops_at_danger_ac006_15_16() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle(snapshot("s1", false));
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("e1", false),
        });
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("d1", true),
        });
        s.handle(AppEvent::ApprovalRequest {
            event: approval_ev("e2", false),
        });
        // A 批量：对 e1 发 allowed-once，danger 前串行续发。
        let cmds = s.handle_command(C::ApprovalBatchAllow);
        assert!(matches!(&cmds[0], Cmd::ReplyApproval { event, .. } if event.event_id == "e1"));
        assert!(s.approval.batch_allow);
        // e1 成功 → 自动续发 d1？不：danger 停点等 ack，batch 自动关闭。
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        assert_eq!(s.approval.event.as_ref().unwrap().event_id, "d1");
        assert!(!s.approval.batch_allow, "danger 停点关闭批量");
        assert!(s.approval.queue.head_requires_ack());
        // ack + y → d1 allowed-once（不提升）。
        s.handle_command(C::ApprovalAlways);
        let cmds = s.handle_command(C::ApprovalAllow);
        assert!(matches!(&cmds[0], Cmd::ReplyApproval { event, .. } if event.event_id == "d1"));
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        // batch 已停 → e2 不自动续发（等待用户决策）。
        assert_eq!(s.approval.event.as_ref().unwrap().event_id, "e2");
        assert_eq!(s.handle_command(C::ApprovalAllow).len(), 1);
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

    // ---------- REQ-006 模型目录 reducer 测试（FR-006-01） ----------

    fn wire_catalog() -> crate::api::types::ModelCatalog {
        serde_json::from_value(serde_json::json!({
            "default": {"provider": "deepseek_official", "model": "deepseek-chat"},
            "routableProviders": ["deepseek_official"],
            "groups": [{
                "id": "deepseek_official",
                "name": "DeepSeek 官方",
                "models": [
                    {"id": "deepseek-chat", "name": "DeepSeek Chat",
                     "reasoning": {"efforts": [{"id": "low", "name": "Low"},
                                               {"id": "high", "name": "High"}],
                                   "defaultEffort": "low"}},
                    {"id": "deepseek-v4-pro", "name": "V4 Pro",
                     "description": "旗舰推理"}
                ]
            }],
            "failures": []
        }))
        .unwrap()
    }

    fn open_catalog_loaded(s: &mut AppState) -> u64 {
        let cmds = s.handle_command(C::OpenModelCatalog);
        let Cmd::FetchModelCatalog { generation } = cmds[0] else {
            panic!("打开目录应发 fetch, 得到 {cmds:?}")
        };
        s.handle(AppEvent::ModelCatalogLoaded {
            generation,
            catalog: wire_catalog(),
        });
        generation
    }

    #[test]
    fn catalog_m_opens_overlay_and_fetches_once_ac006_01() {
        let mut s = AppState::default();
        let cmds = s.handle_command(C::OpenModelCatalog);
        assert_eq!(s.mode, Mode::ModelCatalog);
        assert!(s.model_catalog.visible);
        assert_eq!(s.model_catalog.phase, CatalogPhase::Loading);
        assert_eq!(cmds.len(), 1);
        let Cmd::FetchModelCatalog { generation } = &cmds[0] else {
            panic!("预期拉取目录, 得到 {cmds:?}")
        };
        assert_eq!(*generation, 1, "首次打开 generation=1");
        // 非 NORMAL 打开被忽略。
        s.mode = Mode::Search;
        assert!(s.handle_command(C::OpenModelCatalog).is_empty());
    }

    #[test]
    fn catalog_loaded_indexes_ready_and_local_fuzzy_query_ac006_01_07() {
        let mut s = AppState::default();
        open_catalog_loaded(&mut s);
        assert_eq!(s.model_catalog.phase, CatalogPhase::Ready);
        assert_eq!(s.model_catalog.index.len(), 2);
        // 本地 nucleo 即时过滤（AC-006-01 即时性=本地）。
        s.handle_command(C::PickerInput("v4".into()));
        assert_eq!(s.model_catalog.index.query("v4").len(), 1);
        // 无匹配 → 空态（不误报错误）。
        s.handle_command(C::PickerInput("zzz".into()));
        let hits = s.model_catalog.index.query("zzz");
        assert!(hits.is_empty());
        assert!(s.model_catalog.load_error.is_none(), "空态不误报错误");
        // 退格恢复。
        for _ in 0..5 {
            s.handle_command(C::PickerBackspace);
        }
        assert_eq!(s.model_catalog.index.query("").len(), 2);
    }

    #[test]
    fn catalog_load_failure_shows_error_code_without_auto_retry_ac006_06() {
        let mut s = AppState::default();
        s.handle_command(C::OpenModelCatalog);
        // 权限拒绝：显示 error.code、不自动重试（无 Reconnect/重发 Cmd）。
        let cmds = s.handle(AppEvent::ModelCatalogLoadFailed {
            generation: 1,
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "no permission".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(cmds.is_empty(), "权限错误不自动重试, cmds={cmds:?}");
        assert_eq!(s.model_catalog.phase, CatalogPhase::Error);
        let err = s.model_catalog.load_error.as_deref().unwrap();
        assert!(err.contains("PERMISSION_DENIED"), "err={err}");
        // 恢复路径：Error 态 Enter = 重试加载 → 成功（恢复后重试成功）。
        let cmds = s.handle_command(C::PickerConfirm);
        let Cmd::FetchModelCatalog { generation } = cmds[0] else {
            panic!("Error 态 Enter 应重试, 得到 {cmds:?}")
        };
        assert_eq!(s.model_catalog.phase, CatalogPhase::Loading);
        s.handle(AppEvent::ModelCatalogLoaded {
            generation,
            catalog: wire_catalog(),
        });
        assert_eq!(s.model_catalog.phase, CatalogPhase::Ready);
        assert!(s.model_catalog.load_error.is_none(), "重试成功后清除错误");
    }

    #[test]
    fn catalog_select_no_effort_success_updates_next_and_closes_ac006_08() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        open_catalog_loaded(&mut s);
        // 选中无 efforts 的 v4-pro（本地过滤 + Enter）。
        s.handle_command(C::PickerInput("v4".into()));
        s.handle_command(C::PickerConfirm);
        // selecting 单飞在途。
        assert_eq!(
            s.model_catalog.selecting,
            Some(("deepseek_official".into(), "deepseek-v4-pro".into()))
        );
        // 服务端确认成功。
        let cmds = s.handle(AppEvent::ModelSelected {
            selected: crate::api::types::WireModelSelection {
                provider: "deepseek_official".into(),
                model: "deepseek-v4-pro".into(),
                reasoning_effort: None,
            },
        });
        assert!(cmds.is_empty());
        assert_eq!(s.mode, Mode::Normal, "切换成功后关闭目录回 NORMAL");
        assert!(!s.model_catalog.visible);
        assert!(s.model_catalog.selecting.is_none(), "在途清除");
        assert_eq!(
            s.model_catalog.next_model.as_deref(),
            Some("deepseek_official/deepseek-v4-pro"),
            "next 镜像更新"
        );
        let notice = s.notice.as_deref().unwrap();
        assert!(
            notice.contains("deepseek_official/deepseek-v4-pro"),
            "notice={notice}"
        );
        assert!(notice.contains("下一次 prompt 生效"), "notice={notice}");
    }

    #[test]
    fn catalog_select_failure_keeps_current_and_shows_code_ac006_09() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        open_catalog_loaded(&mut s);
        s.handle_command(C::PickerInput("v4".into()));
        s.handle_command(C::PickerConfirm);
        // 权限拒绝失败：当前模型不变（无 current 变化）、error.code 显示、
        // 无自动重试命令。
        let cmds = s.handle(AppEvent::ModelSelectFailed {
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "no".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(cmds.is_empty(), "权限错误不自动重试, cmds={cmds:?}");
        assert_eq!(
            s.model_catalog.last_error_code.as_deref(),
            Some("PERMISSION_DENIED")
        );
        assert!(s.model_catalog.selecting.is_none(), "在途清除");
        assert_eq!(s.mode, Mode::ModelCatalog, "目录保持打开可继续");
        assert!(s.model_catalog.current_model.is_none(), "当前模型不变");
    }

    #[test]
    fn catalog_select_without_active_session_hints_not_crash() {
        let mut s = AppState::default();
        open_catalog_loaded(&mut s);
        // 过滤到无 efforts 的 v4-pro 再 Enter → 直接 submit 路径（无活动会话
        // 时提示而非崩溃）。
        s.handle_command(C::PickerInput("v4".into()));
        let cmds = s.handle_command(C::PickerConfirm);
        assert!(cmds.is_empty(), "无活动会话不发 selectModel, cmds={cmds:?}");
        assert!(s
            .model_catalog
            .load_error
            .as_deref()
            .unwrap()
            .contains("无打开的会话"));
    }

    #[test]
    fn catalog_effort_subflow_uses_model_declared_efforts() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        open_catalog_loaded(&mut s);
        // 选中带 efforts 的 deepseek-chat（默认 cursor 落在 defaultEffort=low）。
        s.handle_command(C::PickerInput("chat".into()));
        s.handle_command(C::PickerConfirm);
        let pick = s
            .model_catalog
            .effort
            .as_ref()
            .expect("应进入 effort 子阶段");
        assert_eq!(pick.efforts, vec!["low", "high"]);
        assert_eq!(pick.cursor, 0, "default 光标落在 low");
        // effort 阶段输入被忽略（字符不是查询词）。
        s.handle_command(C::PickerInput("x".into()));
        assert_eq!(s.model_catalog.effort.as_ref().unwrap().cursor, 0);
        // j 移动 → high。
        s.handle_command(C::PickerDown);
        assert_eq!(s.model_catalog.effort.as_ref().unwrap().cursor, 1);
        // Esc 从 effort 返回主列表（目录仍开）；再 Enter 重新进入 effort。
        s.handle_command(C::ClosePicker);
        assert!(s.model_catalog.effort.is_none(), "Esc 退回主列表");
        assert!(s.model_catalog.visible, "目录仍开");
        s.handle_command(C::PickerConfirm);
        assert!(s.model_catalog.effort.is_some(), "主列表 Enter 重进 effort");
        s.handle_command(C::PickerDown);
        s.handle_command(C::PickerDown);
        assert_eq!(
            s.model_catalog.effort.as_ref().unwrap().cursor,
            1,
            "光标钳制在末尾"
        );
        // Enter 提交带 effort（确认后 effort 清空）。
        let cmds = s.handle_command(C::PickerConfirm);
        let Cmd::SelectModel {
            provider,
            model,
            reasoning_effort,
        } = &cmds[0]
        else {
            panic!("预期 SelectModel, 得到 {cmds:?}")
        };
        assert_eq!(provider, "deepseek_official");
        assert_eq!(model, "deepseek-chat");
        assert_eq!(reasoning_effort.as_deref(), Some("high"));
        assert!(s.model_catalog.effort.is_none(), "确认后 effort 子阶段结束");
    }

    #[test]
    fn catalog_stale_response_after_reopen_is_dropped() {
        // 生命周期/单飞（模式 15）：关闭后重开 → 旧 generation 的迟到响应
        // 不得污染新目录状态。
        let mut s = AppState::default();
        // 第一次打开（gen=1）。
        let cmds = s.handle_command(C::OpenModelCatalog);
        let Cmd::FetchModelCatalog { generation: g1 } = cmds[0] else {
            panic!()
        };
        // 关闭（作废 gen=1）→ 重开（gen=2）。
        s.handle_command(C::ClosePicker);
        let cmds = s.handle_command(C::OpenModelCatalog);
        let Cmd::FetchModelCatalog { generation: g2 } = cmds[0] else {
            panic!()
        };
        assert_ne!(g1, g2);
        // gen=1 迟到成功 → 丢弃（不进入 Ready，仍是 Loading 等 gen=2）。
        let cmds = s.handle(AppEvent::ModelCatalogLoaded {
            generation: g1,
            catalog: wire_catalog(),
        });
        assert!(cmds.is_empty());
        assert_eq!(s.model_catalog.phase, CatalogPhase::Loading, "旧响应不落位");
        // gen=2 正常到达 → Ready。
        s.handle(AppEvent::ModelCatalogLoaded {
            generation: g2,
            catalog: wire_catalog(),
        });
        assert_eq!(s.model_catalog.phase, CatalogPhase::Ready);
        assert_eq!(s.model_catalog.index.len(), 2);
    }

    // ---------- REQ-006 命令面板 reducer 测试（FR-006-03） ----------

    #[test]
    fn palette_colon_opens_with_remote_fetch_when_session_active() {
        let mut s = AppState::default();
        let cmds = s.handle_command(C::OpenCommandPalette);
        assert_eq!(s.mode, Mode::CommandPalette);
        assert!(s.command_palette.visible);
        // 无活动会话：不拉远端（返回空命令列表）。
        assert!(cmds.is_empty());
        // 打开会话后再开：发 FetchRemoteCommands。
        s.handle_command(C::ClosePicker);
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        let cmds = s.handle_command(C::OpenCommandPalette);
        assert!(matches!(cmds[0], Cmd::FetchRemoteCommands), "cmds={cmds:?}");
        // 远端命令到达 → 缓存并显示为候选。
        s.handle(AppEvent::RemoteCommandsLoaded {
            commands: vec![crate::api::types::CommandDescriptor {
                name: "plan".into(),
                description: "Plan mode".into(),
                input: None,
            }],
        });
        assert!(s.command_palette.remote_fetched);
        let filtered = s.command_palette.filtered();
        assert!(filtered.iter().any(|i| matches!(
            i,
            CommandPaletteItem::Remote { name, .. } if name == "plan"
        )));
    }

    #[test]
    fn palette_confirm_remote_execute_and_failure_stays_open_ac006_13() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.handle_command(C::OpenCommandPalette);
        s.handle(AppEvent::RemoteCommandsLoaded {
            commands: vec![crate::api::types::CommandDescriptor {
                name: "plan".into(),
                description: "Plan mode".into(),
                input: None,
            }],
        });
        // 选中 plan（filtered 里第一个 remote 是 plan? 直接设 query）。
        s.handle_command(C::PickerInput("/plan".into()));
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| matches!(i, CommandPaletteItem::Remote { name, .. } if name == "plan"))
            .unwrap();
        s.command_palette.selection = idx;
        let cmds = s.handle_command(C::PickerConfirm);
        assert!(matches!(&cmds[0], Cmd::ExecuteCommand { line } if line == "/plan"));
        assert!(s.command_palette.executing, "单飞在途");
        // 重复 Enter 单飞拒绝（不重复提交，AC-006-12/15 同源）。
        assert!(s.handle_command(C::PickerConfirm).is_empty());
        // 执行失败：error.code 显示、面板保持打开可继续输入（AC-006-13）。
        let cmds = s.handle(AppEvent::CommandExecuteFailed {
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "denied".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(cmds.is_empty(), "权限错误不自动重试");
        assert_eq!(
            s.command_palette.last_error_code.as_deref(),
            Some("PERMISSION_DENIED")
        );
        assert!(!s.command_palette.executing, "在途清除");
        assert_eq!(s.mode, Mode::CommandPalette, "面板保持打开");
        // 恢复路径：清除错误后执行成功。
        s.command_palette.last_result = None;
        s.handle_command(C::PickerInput("x".into())); // 触发错误清空
        assert!(s.command_palette.last_error_code.is_none());
    }

    #[test]
    fn palette_v04_and_local_actions() {
        let mut s = AppState::default();
        s.handle_command(C::OpenCommandPalette);
        // settings 已是真实动作（V0.4 占位仅剩 keymap/export）。
        let has_settings = s.command_palette.filtered().iter().any(|i| {
            matches!(
                i,
                CommandPaletteItem::Local {
                    label: "settings",
                    ..
                }
            )
        });
        assert!(has_settings, "settings 已转真实入口");
        // 仍为 V0.4 占位项（keymap）：Enter 不执行，仅提示，面板保持。
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| {
                matches!(
                    i,
                    CommandPaletteItem::V04 {
                        label: "keymap",
                        ..
                    }
                )
            })
            .unwrap();
        s.command_palette.selection = idx;
        assert!(s.handle_command(C::PickerConfirm).is_empty());
        assert_eq!(s.mode, Mode::CommandPalette);
        assert!(s
            .command_palette
            .last_result
            .as_deref()
            .unwrap()
            .contains("V0.4"));
        // 本地 model catalog 动作：关闭面板并打开模型目录。
        s.command_palette.query = "model".into();
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| {
                matches!(
                    i,
                    CommandPaletteItem::Local {
                        label: "model catalog",
                        ..
                    }
                )
            })
            .unwrap();
        s.command_palette.selection = idx;
        let cmds = s.handle_command(C::PickerConfirm);
        assert!(matches!(cmds[0], Cmd::FetchModelCatalog { .. }));
        assert_eq!(s.mode, Mode::ModelCatalog, "model catalog 打开");
        // new session 动作：发 CreateSession。
        s.handle_command(C::ClosePicker);
        s.handle_command(C::OpenCommandPalette);
        s.command_palette.query = "new session".into();
        s.command_palette.selection = 0;
        let cmds = s.handle_command(C::PickerConfirm);
        assert!(matches!(cmds[0], Cmd::CreateSession), "cmds={cmds:?}");
        assert_eq!(s.mode, Mode::Normal);
        // SessionCreated → 刷新列表 + notice。
        let cmds = s.handle(AppEvent::SessionCreated {
            session_id: "s-new".into(),
        });
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Cmd::LoadSessionList { .. })));
        assert!(s.notice.as_deref().unwrap().contains("s-new"));
    }

    // ---------- REQ-006 workspace/session 操作（FR-006-02 操作半） ----------

    fn select_palette_item(s: &mut AppState, query: &str) -> usize {
        s.handle_command(C::OpenCommandPalette);
        s.handle_command(C::PickerInput(query.into()));
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| matches!(i, CommandPaletteItem::Local { label, .. } if *label == query))
            .expect("候选存在");
        s.command_palette.selection = idx;
        idx
    }

    #[test]
    fn op_fork_requires_active_session_and_dispatches() {
        let mut s = AppState::default();
        // 无活动会话 → 提示不发。
        select_palette_item(&mut s, "fork session");
        assert!(s.handle_command(C::PickerConfirm).is_empty());
        assert!(s
            .command_palette
            .last_result
            .as_deref()
            .unwrap()
            .contains("无活动会话"));
        // 有活动会话 → fork 直接发（无参数）。
        s.handle_command(C::ClosePicker);
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        select_palette_item(&mut s, "fork session");
        let cmds = s.handle_command(C::PickerConfirm);
        let Cmd::WorkspaceOp { request_id, op } = &cmds[0] else {
            panic!("预期 WorkspaceOp, cmds={cmds:?}")
        };
        assert_eq!(
            op,
            &WorkspaceOperation::ForkSession {
                session_id: SessionId("s1".into())
            }
        );
        assert!(!request_id.is_empty());
        assert_eq!(
            s.command_palette.op_inflight.as_ref().unwrap().0,
            *request_id,
            "requestId 单飞在途"
        );
    }

    #[test]
    fn op_rename_session_input_stage_then_dispatch() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        select_palette_item(&mut s, "rename session");
        assert!(s.handle_command(C::PickerConfirm).is_empty());
        // 进入输入子阶段。
        assert!(matches!(
            s.command_palette.stage,
            Some(PaletteStage::Input {
                kind: OpArgKind::RenameSession,
                ..
            })
        ));
        // 空标题拒绝。
        assert!(s.handle_command(C::PickerConfirm).is_empty());
        assert!(s
            .command_palette
            .last_result
            .as_deref()
            .unwrap()
            .contains("不能为空"));
        // 输入标题 → Enter 提交。
        s.handle_command(C::PickerInput("新标题".into()));
        let cmds = s.handle_command(C::PickerConfirm);
        let Cmd::WorkspaceOp { op, .. } = &cmds[0] else {
            panic!("cmds={cmds:?}")
        };
        assert_eq!(
            op,
            &WorkspaceOperation::RenameSession {
                session_id: SessionId("s1".into()),
                title: "新标题".into()
            }
        );
    }

    #[test]
    fn op_archive_danger_requires_confirm_then_sends_ac006_02() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        select_palette_item(&mut s, "archive session");
        assert!(s.handle_command(C::PickerConfirm).is_empty());
        // 进入 ConfirmDanger（未发送）。
        assert!(matches!(
            s.command_palette.stage,
            Some(PaletteStage::ConfirmDanger { .. })
        ));
        assert!(s.command_palette.op_inflight.is_none(), "确认前不发送");
        // Esc 取消（不执行、面板回列表）。
        s.handle_command(C::ClosePicker);
        assert!(s.command_palette.stage.is_none());
        assert!(s.command_palette.visible);
        // 再次进入并 Enter 确认 → 发送。
        select_palette_item(&mut s, "archive session");
        assert!(s.handle_command(C::PickerConfirm).is_empty());
        let cmds = s.handle_command(C::PickerConfirm);
        let Cmd::WorkspaceOp { op, .. } = &cmds[0] else {
            panic!("确认后应发送, cmds={cmds:?}")
        };
        assert!(
            matches!(op, WorkspaceOperation::ArchiveSession { session_id } if session_id.0 == "s1")
        );
    }

    #[test]
    fn op_success_ack_refreshes_and_failure_keeps_local_no_drift_ac006_10() {
        let mut s = AppState::default();
        s.command_palette.op_inflight = Some((String::from("req-1"), "rename session"));
        let cmds = s.handle(AppEvent::WorkspaceOpDone {
            request_id: "req-1".into(),
            outcome: OpOutcome::Ack,
        });
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Cmd::LoadSessionList { .. })));
        assert!(s.command_palette.op_inflight.is_none());
        // 失败：error.code 显示、不自动重试、本地不漂移。
        s.command_palette.op_inflight = Some((String::from("req-2"), "archive session"));
        let cmds = s.handle(AppEvent::WorkspaceOpFailed {
            request_id: "req-2".into(),
            op_name: "archive session".into(),
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "denied".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(cmds.is_empty(), "失败不发刷新/重试, cmds={cmds:?}");
        assert_eq!(
            s.command_palette.last_error_code.as_deref(),
            Some("PERMISSION_DENIED")
        );
        assert!(s
            .command_palette
            .last_result
            .as_deref()
            .unwrap()
            .contains("archive session 失败"));
        assert!(
            s.command_palette.op_inflight.is_none(),
            "在途清除（可重试）"
        );
    }

    #[test]
    fn op_duplicate_or_late_response_is_idempotent_ac006_12() {
        let mut s = AppState::default();
        s.command_palette.op_inflight = Some((String::from("req-1"), "rename session"));
        // 第一次成功 apply（清在途）。
        let first = s.handle(AppEvent::WorkspaceOpDone {
            request_id: "req-1".into(),
            outcome: OpOutcome::Ack,
        });
        assert_eq!(first.len(), 1, "第一次刷新列表");
        assert!(
            s.notice.as_deref().unwrap().contains("操作成功"),
            "第一次 apply 设 notice"
        );
        let notice_before = s.notice.clone();
        // 重复响应（同 request_id 迟到重放）→ 在途已清 → 不 apply。
        let dup = s.handle(AppEvent::WorkspaceOpDone {
            request_id: "req-1".into(),
            outcome: OpOutcome::ForkCreated {
                session_id: "s-new".into(),
            },
        });
        assert!(dup.is_empty(), "重复响应不重复 apply（不重开 fork）");
        assert_eq!(s.notice, notice_before, "迟到响应不覆盖 notice");
        assert!(
            !matches!(
                s.active_session.as_ref(),
                Some(x) if x.0 == "s-new"
            ),
            "迟到 fork 不打开新会话"
        );
    }

    #[test]
    fn op_fork_success_opens_new_session() {
        let mut s = AppState::default();
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        s.command_palette.op_inflight = Some((String::from("req-9"), "fork session"));
        let cmds = s.handle(AppEvent::WorkspaceOpDone {
            request_id: "req-9".into(),
            outcome: OpOutcome::ForkCreated {
                session_id: "s-fork".into(),
            },
        });
        assert!(cmds.iter().any(|c| {
            matches!(c, Cmd::OpenFollow { session_id, .. } if session_id.0 == "s-fork")
        }));
        assert_eq!(
            s.active_session.as_ref().map(|x| x.0.as_str()),
            Some("s-fork"),
            "fork 成功打开新会话"
        );
    }

    #[test]
    fn reconnect_approval_replay_dedup_and_op_retry_recovers_ac006_18() {
        // AC-006-18：断线重连对账——重放审批/操作不产生重复副作用。
        let mut s = AppState::default();
        // 审批 granted 后事件重放（断线期间 pending，恢复后同事件重到）→
        // 队列 granted 去重集拒绝（AC-006-15/18：不重复授权）。
        s.handle_command(C::OpenSession(SessionId("s1".into())));
        let ev = ApprovalEvent {
            client_id: "c-1".into(),
            event_id: "e-1".into(),
            raw: serde_json::json!({"type": "approval/request", "reason": "deploy"}),
        };
        s.handle(AppEvent::ApprovalRequest { event: ev.clone() });
        assert_eq!(s.approval.event.as_ref().unwrap().event_id, "e-1");
        // 决策已发（ApprovalReplied settle granted）。
        s.handle(AppEvent::ApprovalReplied {
            outcome: ApprovalOutcome::AllowedOnce,
        });
        assert!(s.approval.queue.summary().pending == 0);
        // 断线重连后同事件重放 → 忽略（granted 去重），不再次授权。
        let cmds = s.handle(AppEvent::ApprovalRequest { event: ev });
        assert!(cmds.is_empty(), "重放审批被去重, cmds={cmds:?}");
        assert!(s.approval.event.is_none(), "granted 事件不再进入展示槽");

        // workspace 写操作断线失败（Transport）→ 本地不漂移、在途清除；
        // 恢复后重试成功（失败后修正输入重跑不污染）。
        s.command_palette.op_inflight = Some((String::from("req-r1"), "rename session"));
        let cmds = s.handle(AppEvent::WorkspaceOpFailed {
            request_id: "req-r1".into(),
            op_name: "rename session".into(),
            error: ClientError::Transport("连接断开".into()),
        });
        assert!(cmds.is_empty());
        assert!(
            s.command_palette.op_inflight.is_none(),
            "失败清在途允许重试"
        );
        // 恢复路径：重试成功（requestId 新 id apply）。
        s.command_palette.op_inflight = Some((String::from("req-r2"), "rename session"));
        let cmds = s.handle(AppEvent::WorkspaceOpDone {
            request_id: "req-r2".into(),
            outcome: OpOutcome::Ack,
        });
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Cmd::LoadSessionList { .. })));
    }

    // ---------- REQ-007 V0.4: theme（AC-007-20） ----------

    #[test]
    fn palette_config_applies_overrides_and_reports_warnings() {
        let mut s = AppState::default();
        let mut over = std::collections::BTreeMap::new();
        over.insert("accent".to_string(), "#ff0000".to_string());
        over.insert("error".to_string(), "nope".to_string());
        over.insert("bad_role".to_string(), "#000000".to_string());
        let warnings = s.apply_palette_config("dark", &over);
        assert!(
            warnings.iter().any(|w| w.contains("nope")),
            "warnings={warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("bad_role")),
            "warnings={warnings:?}"
        );
        assert_eq!(
            s.palette.color(crate::ui::theme::Role::Accent),
            ratatui::style::Color::Rgb(255, 0, 0)
        );
        // 非法值回退默认色（不崩）。
        assert_eq!(
            s.palette.color(crate::ui::theme::Role::Error),
            crate::ui::theme::dark_builtin(crate::ui::theme::Role::Error)
        );
    }

    #[test]
    fn palette_default_is_dark_and_apply_light_flips() {
        let s = AppState::default();
        assert!(!s.palette.is_light());
        let mut s = AppState::default();
        let _ = s.apply_palette_config("light", &std::collections::BTreeMap::new());
        assert!(s.palette.is_light());
        assert_eq!(
            s.palette.color(crate::ui::theme::Role::Accent),
            crate::ui::theme::light_builtin(crate::ui::theme::Role::Accent)
        );
    }

    #[test]
    fn toggle_theme_flips_and_emits_save_cmd() {
        let mut s = AppState::default();
        // 打开命令面板选中 theme 本地动作并 Enter。
        s.handle_command(C::OpenCommandPalette);
        s.command_palette.query = "theme".into();
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| matches!(i, CommandPaletteItem::Local { label: "theme", .. }))
            .expect("theme 是本地动作（非 V04 占位）");
        s.command_palette.selection = idx;
        let cmds = s.handle_command(C::PickerConfirm);
        assert!(s.palette.is_light(), "dark→light 翻转");
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SaveUiTheme { .. })),
            "主题切换发出持久化命令"
        );
        assert_eq!(s.mode, Mode::Normal, "面板关闭");
        // 再次切换回到 dark。
        s.handle_command(C::OpenCommandPalette);
        s.command_palette.query = "theme".into();
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| matches!(i, CommandPaletteItem::Local { label: "theme", .. }))
            .unwrap();
        s.command_palette.selection = idx;
        let _ = s.handle_command(C::PickerConfirm);
        assert!(!s.palette.is_light(), "来回切换");
    }

    // ---------- REQ-007 V0.4: draft persistence (AC-007-22/ADR-010) ----------

    #[test]
    fn draft_dirty_flags_and_snapshot_round_trip_ac007_22() {
        let mut s = AppState {
            drafts_enabled: true,
            ..Default::default()
        };
        // 存草稿 → dirty。
        s.drafts.set(DraftState {
            text: "草稿A".into(),
            cursor: 3,
            bound_session: SessionId("s1".into()),
        });
        s.mark_drafts_dirty();
        assert!(s.take_draft_dirty(), "变更后 dirty 置位");
        assert!(!s.take_draft_dirty(), "取出即清");

        // snapshot 到 store（session 键控）。
        let store = s.draft_store_snapshot();
        assert_eq!(store.get("s1"), Some("草稿A"));
        assert!(store.get("s2").is_none());

        // 空文本不落盘、store 往返 toml。
        s.drafts.set(DraftState {
            text: String::new(),
            cursor: 0,
            bound_session: SessionId("s1".into()),
        });
        let toml = s.draft_store_snapshot().to_toml().unwrap();
        assert!(crate::model::DraftStore::from_toml(&toml)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn disabled_drafts_never_dirty_ac007_22() {
        let mut s = AppState {
            drafts_enabled: false,
            ..Default::default()
        };
        s.mark_drafts_dirty();
        assert!(!s.take_draft_dirty(), "disabled 不落盘（纯内存退化）");
    }

    #[test]
    fn seed_and_clear_all_drafts_ac007_22() {
        let mut store = crate::model::DraftStore::default();
        store.set("s1", "草稿一");
        store.set("s2", "草稿二");
        let mut s = AppState::default();
        s.seed_drafts_from_store(store);
        assert!(s.drafts.get(&SessionId("s1".into())).is_some(), "启动恢复");
        assert!(s.drafts.get(&SessionId("s2".into())).is_some());
        // clear：内存全清 + dirty。
        s.clear_all_drafts();
        assert!(s.drafts.is_empty());
        assert!(s.take_draft_dirty());
    }

    #[test]
    fn draft_registry_clear_all_and_session_ids() {
        let mut reg = crate::model::DraftRegistry::new(20);
        reg.set(DraftState {
            text: "a".into(),
            cursor: 0,
            bound_session: SessionId("s1".into()),
        });
        reg.set(DraftState {
            text: "b".into(),
            cursor: 0,
            bound_session: SessionId("s2".into()),
        });
        assert_eq!(reg.session_ids().len(), 2);
        reg.clear_all();
        assert!(reg.is_empty());
    }

    // ---------- REQ-007 V0.4 `:edit`（AC-007-25） ----------

    #[test]
    fn external_edit_begin_requires_open_composer_ac007_25() {
        let mut s = AppState::default();
        // 未打开 composer → 提示不发命令。
        let cmds = s.external_edit_begin();
        assert!(cmds.is_empty());
        assert!(s.notice.as_deref().unwrap_or("").contains("composer"));
    }

    #[test]
    fn external_edit_begin_writes_tmp_and_emits_cmd_ac007_25() {
        // 隔离 $EDITOR（静态锁防并行 env 竞争）。
        static EDITOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = EDITOR_LOCK.lock().unwrap();
        let prev = std::env::var("EDITOR").ok();
        let prev_visual = std::env::var("VISUAL").ok();
        std::env::remove_var("VISUAL");
        std::env::set_var("EDITOR", "/bin/true");
        let mut s = AppState::default();
        let sid = SessionId("sess-e".into());
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: "正在编辑的草稿".into(),
            cursor: 3,
            bound_session: sid.clone(),
        });
        let cmds = s.external_edit_begin();
        let cmd = cmds
            .iter()
            .find(|c| matches!(c, Cmd::ExternalEdit { .. }))
            .expect("发出 ExternalEdit");
        let (tmp, editor) = match cmd {
            Cmd::ExternalEdit { tmp_path, editor } => (tmp_path.clone(), editor.clone()),
            _ => unreachable!(),
        };
        assert_eq!(editor, "/bin/true");
        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            "正在编辑的草稿",
            "草稿写入临时文件"
        );
        assert_eq!(
            s.external_edit.phase,
            crate::model::external_edit::ExternalEditPhase::Editing
        );
        assert_eq!(s.mode, Mode::Normal, "编辑期间退 composer 模态");
        let _ = std::fs::remove_file(&tmp);
        match prev {
            Some(v) => std::env::set_var("EDITOR", v),
            None => std::env::remove_var("EDITOR"),
        }
        match prev_visual {
            Some(v) => std::env::set_var("VISUAL", v),
            None => std::env::remove_var("VISUAL"),
        }
    }

    #[test]
    fn external_edit_done_success_refills_composer_ac007_25() {
        let dir = std::env::temp_dir().join(format!("dshtui-edit-done-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("draft.md");
        let mut s = AppState::default();
        let sid = SessionId("sess-e".into());
        s.composer.visible = false;
        s.draft = Some(DraftState {
            text: "old".into(),
            cursor: 0,
            bound_session: sid.clone(),
        });
        // 模拟 main：suspend + editor 写回 + ExternalEditDone。
        s.external_edit
            .suspend("old", tmp.to_string_lossy().into_owned(), Some("x".into()));
        std::fs::write(&tmp, "EDITED-CONTENT").unwrap();
        s.external_edit_done(true, &tmp, "EDITED-CONTENT", "");
        assert_eq!(
            s.draft.as_ref().map(|d| d.text.as_str()),
            Some("EDITED-CONTENT"),
            "成功回填 composer"
        );
        assert_eq!(s.mode, Mode::Insert);
        assert!(s.composer.visible);
        assert!(!tmp.exists(), "临时文件会话内清理");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_edit_done_failure_keeps_original_draft_ac007_25() {
        let dir = std::env::temp_dir().join(format!("dshtui-edit-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("draft.md");
        let mut s = AppState::default();
        let sid = SessionId("sess-f".into());
        s.composer.visible = false;
        s.draft = Some(DraftState {
            text: "原草稿".into(),
            cursor: 0,
            bound_session: sid.clone(),
        });
        s.external_edit.suspend(
            "原草稿",
            tmp.to_string_lossy().into_owned(),
            Some("bad-editor".into()),
        );
        s.external_edit_done(false, &tmp, "", "编辑器 bad-editor 异常退出（exit 3）");
        assert_eq!(
            s.draft.as_ref().map(|d| d.text.as_str()),
            Some("原草稿"),
            "失败保留原草稿（恢复路径不污染）"
        );
        assert!(s.last_error.as_deref().unwrap_or("").contains(":edit 失败"));
        assert_eq!(s.mode, Mode::Insert);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- REQ-007 V0.4 @ 提及（AC-007-23） ----------

    #[test]
    fn mention_activated_on_at_in_insert_ac007_23() {
        let mut s = AppState::default();
        let sid = SessionId("sess-m".into());
        s.mode = Mode::Insert;
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: "去 ".into(),
            cursor: 3,
            bound_session: sid.clone(),
        });
        // @ 词边界 → 进入 Mention 并发拉取命令。
        let cmds = s.handle_command(crate::input::Command::PickerInput("@".into()));
        assert_eq!(s.mode, Mode::Mention, "进入提及模态");
        assert!(s.mention.active);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::FetchMentionCandidates { .. })),
            "激活即拉候选"
        );
        // 非词边界（@ 在单词中间）不触发。
        s.mention.deactivate();
        s.mode = Mode::Insert;
        s.draft.as_mut().unwrap().text = "foo@".into();
        s.handle_command(crate::input::Command::PickerInput("@".into()));
        assert_eq!(s.mode, Mode::Insert, "非词边界 @ 是普通字符");
    }

    #[test]
    fn mention_query_navigate_confirm_and_close_ac007_23() {
        let mut s = AppState::default();
        let sid = SessionId("sess-m".into());
        s.mode = Mode::Insert;
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: "去 ".into(),
            cursor: 3,
            bound_session: sid.clone(),
        });
        s.handle_command(crate::input::Command::PickerInput("@".into()));
        assert_eq!(s.mode, Mode::Mention);
        // 字符进 query。
        s.handle_command(crate::input::Command::PickerInput("s".into()));
        assert_eq!(s.mention.query, "s");
        // 候选回填（file + session 两源）。
        s.mention.set_candidates(
            s.mention.generation,
            vec![crate::api::types::FileReferenceCandidate {
                path: "src/api/mod.rs".into(),
                kind: "file".into(),
            }],
            vec![crate::api::types::SessionReferenceMentionCandidate {
                session_id: "s1".into(),
                label: "部署".into(),
                cwd: None,
                same_workspace: true,
                created_at: None,
                mention: "@[部署](dsh-session:s1)".into(),
            }],
        );
        assert_eq!(s.mention.filtered().len(), 2);
        // j 移动 + Enter 回填第一候选（文件）——排序 file 在前。
        s.handle_command(crate::input::Command::PickerDown);
        s.handle_command(crate::input::Command::PickerUp);
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert_eq!(s.mode, Mode::Insert, "确认回 INSERT");
        assert!(!s.mention.active);
        assert!(
            s.draft
                .as_ref()
                .map(|d| d.text.as_str())
                .unwrap_or("")
                .contains("src/api/mod.rs"),
            "文件候选回填 composer"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn mention_esc_closes_back_to_insert_ac007_23() {
        let mut s = AppState::default();
        let sid = SessionId("sess-m".into());
        s.mode = Mode::Insert;
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: "".into(),
            cursor: 0,
            bound_session: sid.clone(),
        });
        s.handle_command(crate::input::Command::PickerInput("@".into()));
        assert_eq!(s.mode, Mode::Mention);
        s.handle_command(crate::input::Command::ClosePicker);
        assert_eq!(s.mode, Mode::Insert);
        assert!(!s.mention.active);
        assert!(s.composer.visible, "composer 保留");
    }

    #[test]
    fn mention_fetch_failure_degrades_not_crash_ac007_23() {
        let mut s = AppState::default();
        let sid = SessionId("sess-m".into());
        s.mode = Mode::Insert;
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: "".into(),
            cursor: 0,
            bound_session: sid.clone(),
        });
        s.handle_command(crate::input::Command::PickerInput("@".into()));
        let gen = s.mention.generation;
        let _ = s.handle(AppEvent::MentionCandidatesFailed {
            generation: gen,
            error: ClientError::Transport("断网".into()),
        });
        assert!(!s.mention.loading, "失败停 loading");
        assert_eq!(s.mention.last_error_code.as_deref(), Some("transport"));
        assert_eq!(s.mode, Mode::Mention, "失败不崩，可 Esc 手动输入");
    }

    // ---------- REQ-007 V0.4 图片附件发送（AC-007-24） ----------

    fn temp_img(tag: &str, content: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("dshtui-img-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("photo.png");
        std::fs::write(&file, content).unwrap();
        (dir, file)
    }

    #[test]
    fn submit_with_image_path_line_emits_image_part_ac007_24() {
        use base64::Engine as _;
        let (_dir, file) = temp_img("ok", b"\x89PNG-not-real-but-ok");
        let path = file.to_string_lossy().into_owned();
        let mut s = AppState::default();
        let sid = SessionId("sess-i".into());
        s.mode = Mode::Insert;
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: format!("看这张图\n{path}"),
            cursor: 0,
            bound_session: sid.clone(),
        });
        let cmds = s.submit_input(PromptMode::Queue);
        let cmd = cmds
            .iter()
            .find(|c| matches!(c, Cmd::SendPrompt { .. }))
            .expect("发出发送");
        let (sid2, request) = match cmd {
            Cmd::SendPrompt {
                session_id,
                request,
            } => (session_id, request),
            _ => unreachable!(),
        };
        assert_eq!(sid2, &sid);
        // content 顺序 [image..., text]。
        assert!(
            matches!(&request.content[0], PromptContentPart::Image { .. }),
            "首部为 Image part"
        );
        let text_len = request
            .content
            .iter()
            .filter(|p| matches!(p, PromptContentPart::Text { .. }))
            .count();
        assert_eq!(text_len, 1, "文本部分保留（不含图片行）");
        match &request.content[0] {
            PromptContentPart::Image {
                media_type, data, ..
            } => {
                assert_eq!(media_type, "image/png");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                assert_eq!(decoded, b"\x89PNG-not-real-but-ok");
            }
            _ => unreachable!(),
        }
        assert!(s.pending_image_attachments.is_empty(), "发送后清空在途");
        let _ = std::fs::remove_dir_all(&_dir);
    }

    #[test]
    fn submit_image_read_failure_keeps_draft_in_insert_ac007_24() {
        let mut s = AppState::default();
        let sid = SessionId("sess-i".into());
        s.mode = Mode::Insert;
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: "/nonexistent/nope.png".into(),
            cursor: 0,
            bound_session: sid.clone(),
        });
        let cmds = s.submit_input(PromptMode::Queue);
        assert!(cmds.is_empty(), "失败不发命令");
        assert_eq!(s.mode, Mode::Insert, "保留 INSERT");
        assert!(s.composer.visible, "composer 保留");
        assert_eq!(
            s.draft.as_ref().map(|d| d.text.as_str()),
            Some("/nonexistent/nope.png")
        );
        assert!(s.notice.as_deref().unwrap_or("").contains("图片读取失败"));
        // 恢复路径：修正为有效图片后能正常发送（不被旧失败污染）。
        let (_dir, file) = temp_img("rec", b"abc");
        s.draft.as_mut().unwrap().text = file.to_string_lossy().into_owned();
        s.notice = None;
        let cmds = s.submit_input(PromptMode::Queue);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SendPrompt { .. })),
            "恢复后可发送"
        );
        let _ = std::fs::remove_dir_all(&_dir);
    }

    #[test]
    fn submit_image_over_limit_blocks_with_notice_ac007_24() {
        let (_dir, file) = temp_img("big", b"1234567890");
        let path = file.to_string_lossy().into_owned();
        let mut s = AppState::default();
        let sid = SessionId("sess-i".into());
        s.active_session = Some(sid.clone());
        s.mode = Mode::Insert;
        s.composer.visible = true;
        s.composer.active_session = Some(sid.clone());
        s.draft = Some(DraftState {
            text: path.clone(),
            cursor: 0,
            bound_session: sid.clone(),
        });
        // 造 imageLimits 投影：单张 ≤5 字节 → 超限。
        let window = s.sessions.touch(&sid.0, 50);
        let _ = window.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({
                "imageLimits": {"maxImageBytes": 5, "maxImagesPerMessage": 1,
                                "mediaTypes": ["image/png"]}
            })),
        });
        let cmds = s.submit_input(PromptMode::Queue);
        assert!(cmds.is_empty(), "超限不发");
        assert_eq!(s.mode, Mode::Insert);
        assert!(
            s.notice.as_deref().unwrap_or("").contains("上限"),
            "notice={:?}",
            s.notice
        );
        let _ = std::fs::remove_dir_all(&_dir);
    }

    // ---------- REQ-007 V0.4 subagent 目录（AC-007-07~10） ----------

    #[test]
    fn subagents_open_fetch_and_expand_ac007() {
        let mut s = AppState {
            active_session: Some(SessionId("p1".into())),
            ..Default::default()
        };
        // 打开面板 → fetch 父目录。
        let _ = s.handle_command(crate::input::Command::OpenCommandPalette);
        s.command_palette.query = "subagents".into();
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| {
                matches!(
                    i,
                    CommandPaletteItem::Local {
                        label: "subagents",
                        ..
                    }
                )
            })
            .unwrap();
        s.command_palette.selection = idx;
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert_eq!(s.mode, Mode::Subagent, "进入 subagent 模态");
        assert!(s.subagents.visible);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::FetchSubagentList { .. })),
            "打开即拉取"
        );
        // 拉取回执。
        let gen = 1;
        let _ = s.handle(AppEvent::SubagentListed {
            parent_id: "p1".into(),
            generation: gen,
            catalog: crate::api::types::SubagentCatalog {
                entries: vec![crate::api::types::SubagentListEntry::Child {
                    id: "c1".into(),
                    activity: "running".into(),
                    has_children: true,
                    mode: Some("continuable".into()),
                    label: None,
                }],
                parent_available: true,
            },
        });
        assert!(!s.subagents.loading);
        assert_eq!(s.subagents.roots.len(), 1);
        // Enter 展开 has_children → fetch 子。
        s.subagents.selected = 0;
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Cmd::FetchSubagentList { parent_id, .. } if parent_id == "c1")));
    }

    #[test]
    fn subagents_interrupt_requires_confirm_and_emits_cmd_ac007_09() {
        let mut s = AppState {
            active_session: Some(SessionId("p1".into())),
            ..Default::default()
        };
        s.subagents.open("p1");
        s.mode = Mode::Subagent;
        s.subagents.set_catalog(
            "p1",
            crate::api::types::SubagentCatalog {
                entries: vec![crate::api::types::SubagentListEntry::Child {
                    id: "c1".into(),
                    activity: "running".into(),
                    has_children: false,
                    mode: Some("continuable".into()),
                    label: None,
                }],
                parent_available: true,
            },
        );
        // x → 二次确认态（不发命令）。
        let cmds = s.handle_command(crate::input::Command::SubagentInterrupt);
        assert!(cmds.is_empty(), "确认前不发");
        assert_eq!(s.subagents.interrupt_target.as_deref(), Some("c1"));
        // Esc 取消。
        let _ = s.handle_command(crate::input::Command::ClosePicker);
        assert!(s.subagents.interrupt_target.is_none());
        // 再 x + Enter 确认 → 发 interrupt。
        let _ = s.handle_command(crate::input::Command::SubagentInterrupt);
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::SubagentInterrupt { child_id, parent_id }
                if child_id == "c1" && parent_id == "p1")
            ),
            "确认后发位置参数 interrupt"
        );
    }

    #[test]
    fn subagents_interrupt_failure_surfaces_error_code_ac007_09() {
        let mut s = AppState {
            active_session: Some(SessionId("p1".into())),
            ..Default::default()
        };
        let _ = s.handle(AppEvent::SubagentInterruptDone {
            child_id: "c1".into(),
            error: Some(ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "无权限".into(),
                class: ErrorClass::PermissionDenied,
            }),
        });
        assert_eq!(
            s.subagents.last_error_code.as_deref(),
            Some("PERMISSION_DENIED")
        );
        assert!(!s.subagents.interrupt_target.is_some());
        // 恢复路径：成功回执清错误。
        let _ = s.handle(AppEvent::SubagentInterruptDone {
            child_id: "c1".into(),
            error: None,
        });
        assert_eq!(s.subagents.last_error_code, None);
    }

    #[test]
    fn subagents_list_failed_shows_code_panel_open_ac007_08() {
        let mut s = AppState {
            active_session: Some(SessionId("p1".into())),
            ..Default::default()
        };
        s.subagents.open("p1");
        s.mode = Mode::Subagent;
        let _ = s.handle(AppEvent::SubagentListFailed {
            parent_id: "p1".into(),
            generation: 1,
            error: ClientError::Remote {
                code: "gateway/agent-busy".into(),
                message: "忙".into(),
                class: ErrorClass::UserFacing,
            },
        });
        assert!(!s.subagents.loading);
        assert_eq!(
            s.subagents.last_error_code.as_deref(),
            Some("gateway/agent-busy")
        );
        assert_eq!(s.mode, Mode::Subagent, "失败面板保持可重试/Esc");
    }

    // ---------- REQ-007 V0.4 goal 面板（AC-007-11/12/14） ----------

    #[test]
    fn goal_open_shows_projection_and_empty_state_ac007_11() {
        let mut s = AppState::default();
        let sid = SessionId("sess-g".into());
        s.active_session = Some(sid.clone());
        // 会话有 goal 投影。
        let w = s.sessions.touch(&sid.0, 50);
        let _ = w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({
                "goal": {"goal": {"id": "g1", "revision": 2, "objective": "交付",
                                  "phase": "active"}, "roundsStarted": 1}
            })),
        });
        let _ = s.handle_command(crate::input::Command::OpenCommandPalette);
        s.command_palette.query = "goal".into();
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| matches!(i, CommandPaletteItem::Local { label: "goal", .. }))
            .expect("goal 入口");
        s.command_palette.selection = idx;
        let _cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert_eq!(s.mode, Mode::Goal, "进入 goal 模态");
        assert!(s.goals.visible);
        assert_eq!(
            s.goals.goal.as_ref().map(|g| g.objective.as_str()),
            Some("交付")
        );
        assert_eq!(s.goals.goal.as_ref().map(|g| g.revision), Some(2));
    }

    #[test]
    fn goal_pause_sends_cas_op_and_stale_failure_recovers_ac007_12_14() {
        let mut s = AppState::default();
        let sid = SessionId("sess-g".into());
        s.active_session = Some(sid.clone());
        s.goals.open();
        s.goals.set_goal(
            Some(crate::model::GoalView {
                id: "g1".into(),
                revision: 4,
                objective: "交付".into(),
                phase: Some(crate::api::types::GoalPhase::Active),
                ..Default::default()
            }),
            false,
        );
        s.mode = Mode::Goal;
        // p → pause CAS。
        let cmds = s.handle_command(crate::input::Command::GoalPause);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::GoalOp { .. })),
            "发 pause"
        );
        assert_eq!(s.goals.inflight, Some(crate::model::GoalOpKind::Pause));
        // GOAL_STALE_REVISION 失败 → stale 置位 + 重读提示。
        let _ = s.handle(AppEvent::GoalOpFailed {
            request_id: "x".into(),
            op: GoalMutation::Pause,
            error: ClientError::Remote {
                code: "GOAL_STALE_REVISION".into(),
                message: "stale".into(),
                class: ErrorClass::UserFacing,
            },
        });
        assert!(s.goals.stale_revision);
        assert!(s.goals.inflight.is_none());
        // 恢复：重读投影后（revision 6）再 resume 成功。
        s.goals.set_goal(
            Some(crate::model::GoalView {
                id: "g1".into(),
                revision: 6,
                objective: "交付".into(),
                phase: Some(crate::api::types::GoalPhase::Active),
                ..Default::default()
            }),
            false,
        );
        let cmds = s.handle_command(crate::input::Command::GoalResume);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::GoalOp { .. })),
            "stale 恢复后可重试"
        );
    }

    #[test]
    fn goal_clear_double_confirm_and_create_input_ac007_14() {
        let mut s = AppState::default();
        let sid = SessionId("sess-g".into());
        s.active_session = Some(sid.clone());
        s.goals.open();
        s.goals.set_goal(
            Some(crate::model::GoalView {
                id: "g1".into(),
                revision: 2,
                objective: "交付".into(),
                phase: Some(crate::api::types::GoalPhase::Paused),
                ..Default::default()
            }),
            false,
        );
        s.mode = Mode::Goal;
        // d → confirm（不发）；Enter 确认 → 发 clear。
        let cmds = s.handle_command(crate::input::Command::GoalClear);
        assert!(cmds.is_empty(), "确认前不发");
        let confirm_cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(confirm_cmds.iter().any(|c| matches!(
            c,
            Cmd::GoalOp {
                op: GoalMutation::Clear,
                ..
            }
        )));
        // clear 成功回执 → 单例清空。
        let _ = s.handle(AppEvent::GoalOpDone {
            request_id: "x".into(),
            updated: None,
            cleared: true,
        });
        assert!(s.goals.goal.is_none());
        // 空态 create：c → input，字符 + Enter → 发 Create。
        let _ = s.handle_command(crate::input::Command::GoalCreate);
        assert!(s.goal_input);
        let _ = s.handle_command(crate::input::Command::PickerInput("新目标".into()));
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::GoalOp { op: GoalMutation::Create { objective, .. }, .. } if objective == "新目标")),
            "create 发目标文本"
        );
    }

    // ---------- REQ-007 V0.4 jobs 只读（AC-007-13） ----------

    #[test]
    fn jobs_control_frames_maintain_readonly_mirror_ac007_13() {
        let mut s = AppState::default();
        let sid = SessionId("sess-j".into());
        // baseline.jobs per-session 数组。
        let item = ControlItem::Baseline {
            queues: serde_json::json!([]),
            jobs: serde_json::json!({"sess-j": [
                {"id": "j1", "kind": "tool/call", "label": "跑测试",
                 "status": "running", "startedAt": 1}
            ]}),
            projections: serde_json::json!({"running": true}),
            raw: serde_json::json!({}),
        };
        let _ = s.handle(AppEvent::ControlItem {
            session_id: sid.clone(),
            item,
        });
        assert_eq!(s.jobs.jobs.len(), 1);
        assert_eq!(s.jobs.active_count(), 1);
        // jobs 替换帧：全量替换。
        let item = ControlItem::Jobs {
            jobs: serde_json::json!([
                {"id": "j2", "kind": "k", "label": "lint", "status": "completed"}
            ]),
        };
        let _ = s.handle(AppEvent::ControlItem {
            session_id: sid.clone(),
            item,
        });
        assert_eq!(s.jobs.jobs.len(), 1, "全量替换");
        assert_eq!(s.jobs.jobs[0].id, "j2");
        // 空数组清镜像（不伪造数字）。
        let item = ControlItem::Jobs {
            jobs: serde_json::json!([]),
        };
        let _ = s.handle(AppEvent::ControlItem {
            session_id: sid.clone(),
            item,
        });
        assert!(s.jobs.jobs.is_empty());
    }

    #[test]
    fn jobs_panel_open_move_close_ac007_13() {
        let mut s = AppState::default();
        let _ = s.handle_command(crate::input::Command::OpenCommandPalette);
        s.command_palette.query = "jobs".into();
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| matches!(i, CommandPaletteItem::Local { label: "jobs", .. }))
            .unwrap();
        s.command_palette.selection = idx;
        let _cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert_eq!(s.mode, Mode::Jobs);
        assert!(s.jobs.visible);
        // 无停止键：Esc 关闭回 Normal。
        let _ = s.handle_command(crate::input::Command::ClosePicker);
        assert_eq!(s.mode, Mode::Normal);
        assert!(!s.jobs.visible);
    }

    #[test]
    fn jobs_replacement_frame_full_swap_and_status_chip_data_ac007_13() {
        use crate::api::types::{SessionJob, SessionJobStatus};
        let mut s = AppState::default();
        s.jobs.replace(vec![
            SessionJob {
                id: "j1".into(),
                kind: "k".into(),
                label: "a".into(),
                status: Some(SessionJobStatus::Running),
                ..Default::default()
            },
            SessionJob {
                id: "j2".into(),
                kind: "k".into(),
                label: "b".into(),
                status: Some(SessionJobStatus::Stopping),
                ..Default::default()
            },
            SessionJob {
                id: "j3".into(),
                kind: "k".into(),
                label: "c".into(),
                status: Some(SessionJobStatus::Killed),
                ..Default::default()
            },
        ]);
        assert_eq!(s.jobs.active_count(), 2, "running+stopping 计入");
    }

    // ---------- REQ-007 V0.4 settings + skills（AC-007-15~19） ----------

    #[test]
    fn settings_describe_flattens_whitelist_and_edit_cas_ac007_15_16() {
        let mut s = AppState::default();
        s.open_settings_panel();
        assert_eq!(s.mode, Mode::Settings);
        // describe 回执：白名单行。
        let _ = s.handle(AppEvent::SettingsDescribed {
            value: crate::api::types::SettingsDescribeValue {
                writable: true,
                has_document: true,
                namespaces: vec![crate::api::types::SettingsNamespaceView {
                    ns: "locale".into(),
                    schema: serde_json::json!({}),
                    value: serde_json::json!({"preference": "zh-CN"}),
                    base: None,
                    user: Some(serde_json::json!({"preference": "zh-CN"})),
                    applies: "live".into(),
                    secrets: vec![],
                    revision: 7,
                }],
            },
        });
        assert!(!s.settings.loading);
        assert_eq!(s.settings.rows.len(), 1, "白名单 locale.preference");
        assert_eq!(s.settings.rows[0].value_display, "zh-CN");
        assert!(s.settings.rows[0].user_set);
        // Enter 编辑 → 输入 → Enter 提交 CAS。
        let _ = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(s.settings.edit_key.is_some());
        let _ = s.handle_command(crate::input::Command::PickerBackspace);
        let _ = s.handle_command(crate::input::Command::PickerBackspace);
        let _ = s.handle_command(crate::input::Command::PickerBackspace);
        let _ = s.handle_command(crate::input::Command::PickerBackspace);
        let _ = s.handle_command(crate::input::Command::PickerBackspace);
        let _ = s.handle_command(crate::input::Command::PickerInput("en-US".into()));
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::SettingsUpdate { ns, key, revision, .. }
                if ns == "locale" && key == "preference" && *revision == 7)
            ),
            "CAS revision 上送"
        );
        // 失败（stale/conflict）→ 错误显示 + 清编辑态。
        let _ = s.handle(AppEvent::SettingsUpdateFailed {
            ns: "locale".into(),
            error: ClientError::Remote {
                code: "SETTINGS_STALE_REVISION".into(),
                message: "stale".into(),
                class: ErrorClass::UserFacing,
            },
        });
        assert_eq!(
            s.settings.last_error_code.as_deref(),
            Some("SETTINGS_STALE_REVISION")
        );
        assert!(s.settings.edit_key.is_none());
    }

    #[test]
    fn settings_whitelist_only_and_secret_guard_ac007_16() {
        // flatten_namespace_rows 只产出白名单 key（模块已测）；面板 Enter 在
        // secret/只读行拒绝编辑。
        let mut s = AppState::default();
        s.settings.open();
        s.settings.set_rows(
            vec![crate::model::SettingsRow {
                key: "credentials.token".into(),
                namespace: "credentials".into(),
                value_display: "••• (set)".into(),
                user_set: true,
                secret: true,
                revision: 1,
            }],
            true,
        );
        s.mode = Mode::Settings;
        let _ = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(s.settings.edit_key.is_none(), "secret 行拒绝编辑");
        assert!(s.notice.as_deref().unwrap_or("").contains("只读"));
    }

    #[test]
    fn skills_open_list_copy_ref_and_close_ac007_18() {
        let mut s = AppState::default();
        let sid = SessionId("sess-sk".into());
        s.active_session = Some(sid.clone());
        s.open_skills_panel();
        assert_eq!(s.mode, Mode::Skills);
        let _ = s.handle(AppEvent::SkillsListed {
            value: crate::api::types::SkillListValue {
                skills: vec![crate::api::types::SkillEntry {
                    name: "bash".into(),
                    description: "执行 shell".into(),
                    when_to_use: None,
                    model_invocable: true,
                }],
            },
        });
        assert_eq!(s.skills.items.len(), 1);
        // y 复制引用 → CopyToClipboard。
        let cmds = s.handle_command(crate::input::Command::YankContext);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CopyToClipboard { text } if text == "/bash")),
            "复制 /name"
        );
        // 失败 → error.code，面板保持。
        let _ = s.handle(AppEvent::SkillsListFailed {
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "no".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert_eq!(
            s.skills.last_error_code.as_deref(),
            Some("PERMISSION_DENIED")
        );
        // 关闭。
        let _ = s.handle_command(crate::input::Command::ClosePicker);
        assert_eq!(s.mode, Mode::Normal);
    }

    // ---------- REQ-007 V0.4 会话导出（AC-007-17） ----------

    #[test]
    fn export_open_edit_path_and_start_download_ac007_17() {
        let mut s = AppState {
            active_session: Some(SessionId("sess-x".into())),
            ..Default::default()
        };
        let _ = s.handle_command(crate::input::Command::OpenCommandPalette);
        s.command_palette.query = "export".into();
        let idx = s
            .command_palette
            .filtered()
            .iter()
            .position(|i| {
                matches!(
                    i,
                    CommandPaletteItem::Local {
                        label: "export",
                        ..
                    }
                )
            })
            .unwrap();
        s.command_palette.selection = idx;
        let _cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert_eq!(s.mode, Mode::Export);
        assert!(s.export.visible);
        // 路径编辑：清默认 + 输入。
        for _ in 0..s.export.path.len() {
            let _ = s.handle_command(crate::input::Command::PickerBackspace);
        }
        for c in "/tmp/out.zip".chars() {
            let _ = s.handle_command(crate::input::Command::PickerInput(c.to_string()));
        }
        assert_eq!(s.export.path, "/tmp/out.zip");
        // Enter → 开始下载发命令。
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Cmd::ExportSession { session_id, path }
            if session_id == "sess-x" && path.to_string_lossy() == "/tmp/out.zip")));
        // 成功回执。
        let _ = s.handle(AppEvent::ExportDone {
            bytes: 1234,
            path: std::path::PathBuf::from("/tmp/out.zip"),
        });
        assert!(s.export.phase == crate::model::export::ExportPhase::Done);
    }

    #[test]
    fn export_failure_surfaces_code_and_can_retry_ac007_17() {
        let mut s = AppState {
            active_session: Some(SessionId("sess-x".into())),
            ..Default::default()
        };
        s.export.open("sess-x", "out.zip");
        s.mode = Mode::Export;
        assert!(s.export.begin_download());
        let _ = s.handle(AppEvent::ExportFailed {
            error: ClientError::Remote {
                code: "PERMISSION_DENIED".into(),
                message: "no".into(),
                class: ErrorClass::PermissionDenied,
            },
        });
        assert!(s.export.phase == crate::model::export::ExportPhase::Failed);
        assert_eq!(
            s.export.last_error_code.as_deref(),
            Some("PERMISSION_DENIED")
        );
        // 恢复路径：重开重试可再下载。
        assert!(s.export.begin_download(), "失败后可重试（幂等）");
    }

    #[test]
    fn export_close_during_download_marks_cancel_ac007_17() {
        let mut s = AppState::default();
        s.export.open("sess-x", "out.zip");
        s.mode = Mode::Export;
        assert!(s.export.begin_download());
        let _ = s.handle_command(crate::input::Command::ClosePicker);
        assert!(
            s.notice.as_deref().unwrap_or("").contains("导出已取消"),
            "取消提示, notice={:?}",
            s.notice
        );
        assert_eq!(s.mode, Mode::Normal);
        assert!(!s.export.visible);
    }

    // ---------- REQ-007 V0.4 搜索历史（AC-007-31） ----------

    #[test]
    fn search_history_records_on_close_and_recalls_ac007_31() {
        let mut s = AppState::default();
        // 模拟两轮搜索提交（关闭即记录）。
        s.open_search();
        s.search.query = "/c deploy".into();
        s.recompute_window_matches();
        s.close_search();
        s.open_search();
        s.search.query = "agent".into();
        s.recompute_window_matches();
        s.close_search();
        assert_eq!(s.query_history.len(), 2);
        // 空 query 编辑态 ↑ 回看最近（agent）→ 更早（/c deploy）。
        s.open_search();
        let cmds = s.handle_command(crate::input::Command::PickerUp);
        assert!(cmds.is_empty());
        assert_eq!(s.search.query, "agent");
        let _ = s.handle_command(crate::input::Command::PickerUp);
        assert_eq!(s.search.query, "/c deploy");
        // ↓ 回新 → 再 ↓ 越界清空。
        let _ = s.handle_command(crate::input::Command::PickerDown);
        assert_eq!(s.search.query, "agent");
        let _ = s.handle_command(crate::input::Command::PickerDown);
        assert_eq!(s.search.query, "", "越过最新清空");
        assert!(s.search.recall_cursor.is_none());
        // 非空 query 时 ↑ 不回忆（保持输入语义）。
        s.search.query = "deploy".into();
        let _ = s.handle_command(crate::input::Command::PickerUp);
        assert_eq!(s.search.query, "deployk", "非空 ↑ 保持既有 'k' 输入语义");
    }

    #[test]
    fn search_history_empty_no_recall_and_unchanged_semantics() {
        let mut s = AppState::default();
        s.open_search();
        let cmds = s.handle_command(crate::input::Command::PickerUp);
        assert!(cmds.is_empty());
        assert_eq!(s.search.query, "", "无历史不回忆");
        s.close_search();
    }

    // ---------- REQ-007 V0.4 消息动作（AC-007-27/28） ----------

    fn msg_app_with_blocks(
        rows: Vec<(u64, &'static str, Option<&'static str>)>,
    ) -> (AppState, SessionId) {
        // rows: (seq, "user/message"|"assistant/message", content/message_id)
        let mut s = AppState::default();
        let sid = SessionId("sess-ma".into());
        s.active_session = Some(sid.clone());
        let w = s.sessions.touch(&sid.0, 50);
        let mut records = Vec::new();
        for (seq, typ, payload) in rows {
            let data = if typ == "user/message" {
                serde_json::json!({"content": payload.unwrap_or("hi")})
            } else {
                serde_json::json!({"id": payload.unwrap_or("m1")})
            };
            records.push(SessionHistoryRecord::Event {
                event: SessionWireEvent {
                    event_type: typ.into(),
                    seq: Some(SessionSeq(seq)),
                    time: None,
                    request_id: None,
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                    data: Some(data),
                },
            });
        }
        let _ = w.apply(Incoming::Snapshot {
            cursor: None,
            records,
            has_more: false,
            projections: None,
        });
        (s, sid)
    }

    #[test]
    fn message_action_user_open_retry_and_branch_ac007_27() {
        // 末条 user（seq 3）+ assistant（seq 5）。
        let (mut s, sid) = msg_app_with_blocks(vec![
            (1, "user/message", Some("你好")),
            (5, "assistant/message", Some("m5")),
            (6, "user/message", Some("再来一次")),
        ]);
        // cursor 定位到末条 user（window 内块下标 2）。
        s.cursor_block = 2;
        let cmds = s.handle_command(crate::input::Command::OpenMessageActions);
        assert!(cmds.is_empty());
        assert_eq!(s.mode, Mode::MessageAction);
        assert_eq!(s.message_action.menu_seq, Some(6));
        // 动作 = [retry, branch]（末条静止 user）；默认 cursor 0 = retry。
        // Enter → retry。
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SendPrompt { session_id, .. } if session_id == &sid)),
            "retry 重发 prompt"
        );
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn message_action_branch_at_last_user_ac007_27() {
        let (mut s, sid) = msg_app_with_blocks(vec![
            (1, "user/message", Some("你好")),
            (5, "assistant/message", Some("m5")),
            (6, "user/message", Some("再来一次")),
        ]);
        s.cursor_block = 2;
        let _ = s.handle_command(crate::input::Command::OpenMessageActions);
        // 移到 branch（index 1）并 Enter。
        let _ = s.handle_command(crate::input::Command::PickerDown);
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ForkAtSeq { session_id, at_seq }
                if session_id == &sid.0 && *at_seq == 6)),
            "branch fork atSeq=6"
        );
    }

    #[test]
    fn message_action_assistant_feedback_put_ac007_27() {
        let (mut s, _sid) = msg_app_with_blocks(vec![
            (1, "user/message", Some("你好")),
            (5, "assistant/message", Some("m5")),
        ]);
        s.cursor_block = 1; // assistant
        let _ = s.handle_command(crate::input::Command::OpenMessageActions);
        assert_eq!(s.message_action.menu_seq, Some(5));
        // actions = [feedback+, feedback-]; Enter → feedback+。
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::FeedbackPut { message_id, rating, .. }
                if message_id == "m5" && rating == "positive")),
            "feedback+ put m5"
        );
    }

    #[test]
    fn message_action_running_turn_requires_double_confirm_ac007_28() {
        let (mut s, sid) = msg_app_with_blocks(vec![(1, "user/message", Some("你好"))]);
        s.running_sessions.insert(sid.clone());
        s.cursor_block = 0;
        let _ = s.handle_command(crate::input::Command::OpenMessageActions);
        // running：不可 branch；仅 retry。第一次 Enter → confirm 态不发。
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(cmds.is_empty(), "运行中第一次 Enter 不执行");
        assert!(s.message_action.confirm_running);
        assert_eq!(s.message_action.menu_seq, Some(1));
        // 第二次 Enter → 确认执行 retry。
        let cmds = s.handle_command(crate::input::Command::PickerConfirm);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SendPrompt { session_id, .. } if session_id == &sid)),
            "二次确认后执行"
        );
        // Esc 取消路径：重新打开 → Esc 清态。
        let _ = s.handle_command(crate::input::Command::OpenMessageActions);
        let _ = s.handle_command(crate::input::Command::PickerConfirm); // confirm 态
        assert!(s.message_action.confirm_running);
        let _ = s.handle_command(crate::input::Command::ClosePicker);
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.message_action.menu_seq.is_none());
        assert!(s.msg_action_target.is_none());
    }

    #[test]
    fn message_action_branch_failure_shows_code_ac007_27() {
        let mut s = AppState {
            active_session: Some(SessionId("sess-x".into())),
            ..Default::default()
        };
        let _ = s.handle(AppEvent::MessageActionFailed {
            op: crate::model::MessageActionKind::Branch,
            error: ClientError::Remote {
                code: "session/fork-unavailable".into(),
                message: "不可用".into(),
                class: ErrorClass::UserFacing,
            },
        });
        assert!(s.notice.as_deref().unwrap_or("").contains("branch 失败"));
        assert_eq!(
            s.message_action.last_error_code.as_deref(),
            Some("session/fork-unavailable")
        );
    }
}
