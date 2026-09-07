//! Vim-like keymap for the V0.1 input contract (`Notes/04-ui-ux-design.md §3`).
//!
//! The decoder is deliberately pure: it consumes crossterm events and emits
//! domain commands; it never mutates AppState or talks to the transport.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Input modes that affect key meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum InputMode {
    #[default]
    Normal,
    Picker,
    Insert,
    Help,
    /// `/` 结构化搜索 overlay（REQ-003）。
    Search,
    /// `v`/`V` 视觉选择（REQ-003）。
    Visual,
    /// 审批弹窗（REQ-003，y/n/q/Esc 决策）。
    Approval,
    /// 审批列表视图（REQ-006，`L` 打开；j/k 移动、r 重试失败项、A 批量）。
    ApprovalList,
    /// REQ-004 V0.2：IMAGEVIEW 模式（仅 Kitty 渲染态出现，D-14）。
    ImageView,

    /// REQ-005 V0.3：Trajectory 视图（D-25：独立模式；详情为内嵌焦点子层，
    /// 不新增 InputMode——`q`/`y`/`j`/`k` 由 AppState 按 focus 分派）。
    Trajectory,
    /// REQ-005：轨迹内过滤输入态（`/` 打开后任意字符进 query；仍在
    /// Trajectory 模式，模态上不离开轨迹 tab）。
    TrajectoryFilter,
    /// REQ-006：模型目录 overlay（`M` 打开；输入即时本地 nucleo 过滤，
    /// j/k 移动、Enter 选择、q/Esc 关闭）。effort 子阶段同键位表。
    ModelCatalog,
    /// REQ-006：命令面板 overlay（`:` 打开；输入过滤、j/k 移动、Enter 执行、
    /// Esc/q 关闭）。
    CommandPalette,
    /// REQ-009 V0.3：MONITOR 模式（`dshtui monitor` 独立键位表）。
    Monitor,
    /// REQ-007 V0.4：@ 提及候选（composer INSERT 内 `@` 触发；AC-007-23）。
    Mention,
    /// REQ-007 V0.4：subagent 目录面板（`:subagents`；AC-007-07~10）。
    Subagent,
}

/// Domain commands emitted by the input layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    MoveDown,
    MoveUp,
    HalfPageDown,
    HalfPageUp,
    GotoBottom,
    GotoTop,
    OpenPicker,
    ClosePicker,
    InsertMode,
    PickerDown,
    PickerUp,
    PickerConfirm,
    PickerInput(String),
    PickerBackspace,
    SubmitInput,
    OpenSelected,
    /// NORMAL 模式 Enter：打开焦点块（图片占位 → 打开 ImageView / 系统查看器）。
    OpenFocused,
    OpenHelp,
    CloseHelp,
    Quit,
    RetryProbe,
    CycleFocus,
    ToggleWorkspace,
    StopRunning,
    CollapseProject,
    ExpandProject,
    OpenSession(crate::api::types::SessionId),
    Resize {
        width: u16,
        height: u16,
    },
    // ---------- REQ-003：搜索/视觉/审批/大纲/历史 ----------
    /// `/` 打开 SEARCH overlay。
    StartSearch,
    /// SEARCH 中 `n`/`N` 巡览窗口命中。
    SearchNext,
    SearchPrev,
    /// `v`/`V` 进入视觉选择（char / line）。
    VisualStart {
        line: bool,
    },
    /// VISUAL/SEARCH 中 `y` 复制（上下文 yank）。
    VisualYank,
    /// NORMAL `y` 上下文 yank（代码块/链接/图片/工具结果/段落）。
    YankContext,
    /// `O` 打开 turnOutline 大纲列表（D-19 独立键）。
    OpenOutline,
    /// `]`/`[` 跳下一/上一轮（turnOutline + loadThrough）。
    NextTurn,
    PrevTurn,
    /// APPROVAL 决策键（REQ-003 §3）。
    ApprovalAllow,
    ApprovalReject,
    ApprovalCancel,
    ApprovalAlways,
    // ---------- REQ-006 审批队列增强（D-036） ----------
    /// APPROVAL 单条槽中 `L`：打开审批列表视图。
    OpenApprovalList,
    /// ApprovalList 中 `r`：重试光标行失败项。
    ApprovalRetry,
    /// ApprovalList 中 `A`：批量 allowed-once（串行泵自动续发）。
    ApprovalBatchAllow,
    /// ApprovalList 中 q/Esc：回单条槽不中止。
    CloseApprovalList,
    /// INSERT `↑`/`↓` 输入历史（REQ-F06）。
    HistoryPrev,
    HistoryNext,
    // ---------- REQ-004 IMAGEVIEW 级键位（D-14） ----------
    /// `y`：复制图片路径/附件名。
    ImageViewCopy,
    /// `o`：系统查看器打开原图。
    ImageViewOpenExternal,
    /// `q`：关闭 ImageView 回 transcript（NORMAL）。
    ImageViewClose,
    // ---------- REQ-005 Trajectory 级键位（D-25/Notes/04 §3.6） ----------
    /// `gt`（Normal→Trajectory，Trajectory→Chat）：顶部 Tab 切换。
    ToggleTrajectory,
    /// `gT` / `1`：切回 Chat Tab。
    GotoChat,
    /// `z` / `za`：折叠/展开 turn、assistant 组。
    ToggleFold,
    /// `Enter` / `d`：打开选中事件详情（右栏子层）。
    OpenDetail,
    // ---------- REQ-009 MONITOR 级键位（FR-009-04） ----------
    /// `c`：对焦点 agent 打开问答（`/agent/chat`）。
    MonitorOpenChat,
    /// `s`：打开 KB 统计 pane。
    MonitorStats,
    /// `f`：加油动效（颜色脉冲 + 状态提示）。
    MonitorCheer,
    /// `l`：定位焦点 agent（跳转 NPC + 状态提示）。
    MonitorLocate,
    // ---------- REQ-006 模型目录（FR-006-01） ----------
    /// NORMAL `M`：打开模型目录 overlay（本地 nucleo 过滤 + 热切换）。
    OpenModelCatalog,
    // ---------- REQ-006 侧栏视图（FR-006-02，D-034） ----------
    /// `gv`：循环切换侧栏 groupBy/orderBy（仅本地视图态，无远端写）。
    CycleSidebarView,
    // ---------- REQ-006 命令面板（FR-006-03） ----------
    /// NORMAL `:`：打开命令面板（本地命令 + 斜杠命令）。
    OpenCommandPalette,
    /// INSERT 中 `Tab`：呼出命令面板并预填当前 `/` 斜杠命令词（补全）。
    ComposerTabComplete,
    // ---------- REQ-007 V0.4 subagent（AC-007-07~10） ----------
    /// subagent 目录面板 `x`：请求中断所选子代理（二次确认后执行）。
    SubagentInterrupt,
}

/// Stateful decoder for multi-key Normal-mode commands such as `gg`.
#[derive(Debug, Clone, Default)]
pub struct KeyDecoder {
    pending_g: bool,
    /// 非 None 时启用 `[keymap]` 覆盖层（AC-007-21）。默认 None = 内置键位，
    /// decode 与历史硬编码分支逐位一致（向后兼容）。
    keymap: Option<Keymap>,
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 携带 `[keymap]` 覆盖的解码器（REQ-007 AC-007-21）。
    pub fn with_keymap(keymap: Keymap) -> Self {
        Self {
            pending_g: false,
            keymap: Some(keymap),
        }
    }

    /// 从 effective 配置构造：`[keymap]` 空 → 内置键位（`new()` 同构）；
    /// 非空 → build + 输出可读警告（tracing）。
    pub fn from_effective(eff: &crate::config::Effective) -> Self {
        if eff.keymap.modes.is_empty() {
            return Self::new();
        }
        let km = Keymap::build(&eff.keymap);
        for w in &km.warnings {
            tracing::warn!(warning = %w, "[keymap] 覆盖警告");
        }
        Self::with_keymap(km)
    }

    pub fn reset(&mut self) {
        self.pending_g = false;
    }

    /// Translate one crossterm event. Key releases and unsupported events are ignored.
    pub fn decode(&mut self, mode: InputMode, event: Event) -> Option<Command> {
        match event {
            Event::Resize(width, height) => Some(Command::Resize { width, height }),
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // REQ-007：`[keymap]` 覆盖层（Step 5，keymap_override_proto 验证）。
                if let Some(km) = &self.keymap {
                    if let Some(outcome) = km.decode_override(mode, key) {
                        // 非 `g` 键命中原默认键需清 pending_g（与原分支语义一致）；
                        // `g` 前缀键永不进 override 表。
                        self.pending_g = false;
                        return outcome;
                    }
                }
                self.decode_key(mode, key)
            }
            _ => None,
        }
    }

    fn decode_key(&mut self, mode: InputMode, key: KeyEvent) -> Option<Command> {
        match mode {
            InputMode::Normal => self.normal(key),
            InputMode::Picker => self.picker(key),
            InputMode::Insert => self.insert(key),
            InputMode::Help => self.help(key),
            InputMode::Search => self.search(key),
            InputMode::Visual => self.visual(key),
            InputMode::Approval => self.approval(key),
            InputMode::ApprovalList => self.approval_list(key),
            InputMode::ImageView => self.image_view(key),
            InputMode::Trajectory => self.trajectory(key),
            InputMode::TrajectoryFilter => self.trajectory_filter(key),
            InputMode::ModelCatalog => self.model_catalog(key),
            InputMode::CommandPalette => self.command_palette(key),
            InputMode::Monitor => self.monitor(key),
            InputMode::Mention => self.mention(key),
            InputMode::Subagent => self.subagent(key),
        }
    }

    /// `g` 前缀在按下 g 时置位；第二键 t/T/gg 触发 Tab/顶部命令，其它键清
    /// 前缀后按普通单键解码（g 后 j → MoveDown，回归语义）。
    fn g_prefix_key(&mut self, c: char) -> Option<Command> {
        match c {
            't' => Some(Command::ToggleTrajectory),
            'T' => Some(Command::GotoChat),
            // REQ-006：`gv` 循环侧栏视图（groupBy/orderBy，D-034）。
            'v' => Some(Command::CycleSidebarView),
            _ => None,
        }
    }

    fn normal(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('g') if !ctrl => {
                if self.pending_g {
                    self.pending_g = false;
                    Some(Command::GotoTop)
                } else {
                    self.pending_g = true;
                    None
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if self.pending_g {
                    if let Some(cmd) = self.g_prefix_key(c) {
                        // gt/gT/gv 等双键命令：清前缀（g 后继续按 g 不再误触
                        // GotoTop；code-review 同源修复）。
                        self.pending_g = false;
                        return Some(cmd);
                    }
                    self.pending_g = false; // g+其它键：清前缀，按单键解码
                }
                match c {
                    'j' => Some(Command::MoveDown),
                    'k' => Some(Command::MoveUp),
                    'G' => Some(Command::GotoBottom),
                    'f' => Some(Command::OpenPicker),
                    'i' => Some(Command::InsertMode),
                    '?' => Some(Command::OpenHelp),
                    'q' => Some(Command::Quit),
                    'r' => Some(Command::RetryProbe),
                    's' => Some(Command::StopRunning),
                    'h' => Some(Command::CollapseProject),
                    'l' => Some(Command::ExpandProject),
                    'o' => Some(Command::OpenSelected),
                    // REQ-003 键位（REQ §3 输入契约，D-19 `O` 独立键）。
                    '/' => Some(Command::StartSearch),
                    'v' => Some(Command::VisualStart { line: false }),
                    'V' => Some(Command::VisualStart { line: true }),
                    'y' => Some(Command::YankContext),
                    'O' => Some(Command::OpenOutline),
                    ']' => Some(Command::NextTurn),
                    '[' => Some(Command::PrevTurn),
                    // REQ-005 Tab 数字键（`1` Chat / `2` Trajectory）。
                    '1' => Some(Command::GotoChat),
                    '2' => Some(Command::ToggleTrajectory),
                    // REQ-006 模型目录（FR-006-01；`M` 现未占用）。
                    'M' => Some(Command::OpenModelCatalog),
                    // REQ-006 命令面板（FR-006-03；Notes/04 §3.1 `:`）。
                    ':' => Some(Command::OpenCommandPalette),
                    _ => None,
                }
            }
            KeyCode::Char(c) => {
                self.pending_g = false;
                match (c, ctrl) {
                    ('d', true) => Some(Command::HalfPageDown),
                    ('u', true) => Some(Command::HalfPageUp),
                    ('c', true) => Some(Command::Quit),
                    ('w', true) => Some(Command::CycleFocus),
                    _ => None,
                }
            }
            KeyCode::Enter if !ctrl => {
                self.pending_g = false;
                Some(Command::OpenFocused)
            }
            KeyCode::Esc => {
                self.pending_g = false;
                None
            }
            _ => {
                self.pending_g = false;
                None
            }
        }
    }

    /// REQ-005 Trajectory 键位（D-25/Notes/04 §3.6）：j/k 事件行上下、
    /// z/za 折叠、Enter/d 详情、/ 轨迹内过滤、y 复制、q 退出（详情子层语义
    /// 在 AppState 按 focus 分派）、gt/gT/数字切 Tab、Ctrl+w 焦点循环。
    fn trajectory(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('g') if !ctrl => {
                if self.pending_g {
                    self.pending_g = false;
                    Some(Command::GotoTop)
                } else {
                    self.pending_g = true;
                    None
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if self.pending_g {
                    if let Some(cmd) = self.g_prefix_key(c) {
                        // Trajectory 内 gt/gT/gv：清前缀（同 normal 修复）。
                        self.pending_g = false;
                        return Some(cmd);
                    }
                    self.pending_g = false;
                }
                match c {
                    'j' => Some(Command::MoveDown),
                    'k' => Some(Command::MoveUp),
                    'z' => Some(Command::ToggleFold),
                    'd' => Some(Command::OpenDetail),
                    '/' => Some(Command::StartSearch),
                    'y' => Some(Command::YankContext),
                    'q' => Some(Command::Quit),
                    '1' => Some(Command::GotoChat),
                    '2' => Some(Command::ToggleTrajectory),
                    'G' => Some(Command::GotoBottom),
                    _ => None,
                }
            }
            KeyCode::Char('c') if ctrl => {
                // S8 修复：非 g 前缀键路径清 pending_g（详情打开后按 t 不再
                // 误触 gt 切回 Chat；与 normal 模式 Enter/Esc 清理同构）。
                self.pending_g = false;
                Some(Command::Quit)
            }
            KeyCode::Char('w') if ctrl => {
                self.pending_g = false;
                Some(Command::CycleFocus)
            }
            KeyCode::Enter if !ctrl => {
                self.pending_g = false;
                Some(Command::OpenDetail)
            }
            KeyCode::Esc => {
                self.pending_g = false;
                Some(Command::ClosePicker)
            }
            _ => {
                self.pending_g = false;
                None
            }
        }
    }

    /// 轨迹内过滤输入态键位：普通字符进 query，Backspace 删除，Enter 跳转，
    /// Esc/q 退出，j/k 命中选中移动（Step 4，AC-005-05/13）。
    /// 注意匹配臂顺序：特殊键必须排在兜底 `Char(c)` 之前，否则被当查询
    /// 字符吞掉（code-review S1 修复，与 picker 模式同构）。
    fn trajectory_filter(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Some(Command::Quit),
            KeyCode::Enter if !ctrl => Some(Command::PickerConfirm),
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Char('q') if !ctrl => Some(Command::ClosePicker),
            KeyCode::Char('j') if !ctrl => Some(Command::MoveDown),
            KeyCode::Char('k') if !ctrl => Some(Command::MoveUp),
            KeyCode::Backspace => Some(Command::PickerBackspace),
            KeyCode::Char(c) if !ctrl => Some(Command::PickerInput(c.to_string())),
            _ => None,
        }
    }

    fn picker(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Enter => Some(Command::PickerConfirm),
            KeyCode::Up => Some(Command::PickerUp),
            KeyCode::Down => Some(Command::PickerDown),
            KeyCode::Char('k') => Some(Command::PickerUp),
            KeyCode::Char('j') => Some(Command::PickerDown),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Command::PickerUp)
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Command::PickerDown)
            }
            KeyCode::Backspace => Some(Command::PickerBackspace),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Command::PickerInput(c.to_string()))
            }
            _ => None,
        }
    }

    fn insert(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Enter if key.modifiers.is_empty() => Some(Command::SubmitInput),
            // REQ-002 FR-002-01: Ctrl+Enter / Alt+Enter insert a newline
            // (either modifier).
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Some(Command::PickerInput("\n".into()))
            }
            // Ctrl+c keeps the global quit path while composing (AC-002-07).
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Command::Quit)
            }
            KeyCode::Char(c) if key.modifiers.is_empty() => {
                Some(Command::PickerInput(c.to_string()))
            }
            KeyCode::Backspace => Some(Command::PickerBackspace),
            // REQ-003 AC-003-10：INSERT 中 ↑/↓ 输入历史（全局最近 50 条）。
            KeyCode::Up => Some(Command::HistoryPrev),
            KeyCode::Down => Some(Command::HistoryNext),
            // REQ-006 FR-006-03：INSERT 中 Tab 呼出命令面板（预填当前
            // `/` 斜杠命令词，Notes/04 §3.2 命令/路径补全）。
            KeyCode::Tab => Some(Command::ComposerTabComplete),
            _ => None,
        }
    }

    /// SEARCH overlay 键位（REQ-003 §3.3）：输入实时过滤，Enter 跳转，
    /// n/N 巡览，j/k 选中历史命中，y 复制命中，Esc 退出。
    fn search(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Enter if key.modifiers.is_empty() => Some(Command::PickerConfirm),
            KeyCode::Backspace => Some(Command::PickerBackspace),
            KeyCode::Up => Some(Command::PickerUp),
            KeyCode::Down => Some(Command::PickerDown),
            KeyCode::Char('j') if key.modifiers.is_empty() => Some(Command::PickerDown),
            KeyCode::Char('k') if key.modifiers.is_empty() => Some(Command::PickerUp),
            KeyCode::Char('n') if key.modifiers.is_empty() => Some(Command::SearchNext),
            KeyCode::Char('N') if key.modifiers.is_empty() => Some(Command::SearchPrev),
            KeyCode::Char('y') if key.modifiers.is_empty() => Some(Command::YankContext),
            KeyCode::Char(c) if key.modifiers.is_empty() => {
                Some(Command::PickerInput(c.to_string()))
            }
            _ => None,
        }
    }

    /// VISUAL 键位（Notes/04 §3.1）：j/k 扩展选择，y 复制，o 打开选中链接，
    /// Esc/v/V 退出。
    fn visual(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Char('v') if key.modifiers.is_empty() => Some(Command::ClosePicker),
            KeyCode::Char('V') if key.modifiers.is_empty() => Some(Command::ClosePicker),
            KeyCode::Char('j') if key.modifiers.is_empty() => Some(Command::MoveDown),
            KeyCode::Char('k') if key.modifiers.is_empty() => Some(Command::MoveUp),
            KeyCode::Char('y') if key.modifiers.is_empty() => Some(Command::YankContext),
            KeyCode::Char('o') if key.modifiers.is_empty() => Some(Command::OpenSelected),
            _ => None,
        }
    }

    /// APPROVAL 键位（REQ-003 §3.5 + REQ-006 D-036）：y 允许 / n 拒绝 /
    /// q·Esc 中止（cancelled）；a 危险项风险确认（否则始终允许指引）；
    /// L 打开审批列表。Enter 无语义（§3 键位边界）。
    fn approval(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ApprovalCancel),
            KeyCode::Char('y') if key.modifiers.is_empty() => Some(Command::ApprovalAllow),
            KeyCode::Char('n') if key.modifiers.is_empty() => Some(Command::ApprovalReject),
            KeyCode::Char('q') if key.modifiers.is_empty() => Some(Command::ApprovalCancel),
            KeyCode::Char('a') if key.modifiers.is_empty() => Some(Command::ApprovalAlways),
            KeyCode::Char('L') if key.modifiers.is_empty() => Some(Command::OpenApprovalList),
            _ => None,
        }
    }

    /// REQ-006 ApprovalList 键位（D-036）：j/k 移动、r 重试失败项、
    /// A 批量 allowed-once、q/Esc 回单条槽不中止（不发出 cancelled）。
    fn approval_list(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::CloseApprovalList),
            KeyCode::Char('q') if key.modifiers.is_empty() => Some(Command::CloseApprovalList),
            KeyCode::Char('j') if key.modifiers.is_empty() => Some(Command::PickerDown),
            KeyCode::Char('k') if key.modifiers.is_empty() => Some(Command::PickerUp),
            KeyCode::Char('r') if key.modifiers.is_empty() => Some(Command::ApprovalRetry),
            KeyCode::Char('A') if key.modifiers.is_empty() => Some(Command::ApprovalBatchAllow),
            // 列表视图中 y/n 仍对单条槽（active）决策（批量便捷键之外保留单条）。
            KeyCode::Char('y') if key.modifiers.is_empty() => Some(Command::ApprovalAllow),
            KeyCode::Char('n') if key.modifiers.is_empty() => Some(Command::ApprovalReject),
            KeyCode::Char('a') if key.modifiers.is_empty() => Some(Command::ApprovalAlways),
            KeyCode::Char('L') if key.modifiers.is_empty() => Some(Command::CloseApprovalList),
            _ => None,
        }
    }

    /// 模型目录 overlay 键位（REQ-006 FR-006-01，D-032/ADR-003）：普通字符
    /// 输入 query（本地即时过滤）、j/k 移动命中、Enter 选择（effort 子阶段
    /// 确认 effort）、Esc/q 关闭、Backspace 删字符。effort 子阶段由 reducer
    /// 依状态分派 Enter/Esc（同键位表，无独立 InputMode）。
    fn model_catalog(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Char('q') if key.modifiers.is_empty() => Some(Command::ClosePicker),
            KeyCode::Enter if key.modifiers.is_empty() => Some(Command::PickerConfirm),
            KeyCode::Char('j') if key.modifiers.is_empty() => Some(Command::PickerDown),
            KeyCode::Char('k') if key.modifiers.is_empty() => Some(Command::PickerUp),
            KeyCode::Backspace => Some(Command::PickerBackspace),
            KeyCode::Char(c) if key.modifiers.is_empty() => {
                Some(Command::PickerInput(c.to_string()))
            }
            _ => None,
        }
    }

    /// 命令面板 overlay 键位（REQ-006 FR-006-03）：字符输入命令名/斜杠行、
    /// j/k 移动候选、Enter 执行、Backspace 删、Esc/q 关闭。
    fn command_palette(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Char('q') if key.modifiers.is_empty() => Some(Command::ClosePicker),
            KeyCode::Enter if key.modifiers.is_empty() => Some(Command::PickerConfirm),
            KeyCode::Char('j') if key.modifiers.is_empty() => Some(Command::PickerDown),
            KeyCode::Char('k') if key.modifiers.is_empty() => Some(Command::PickerUp),
            KeyCode::Backspace => Some(Command::PickerBackspace),
            KeyCode::Char(c) if key.modifiers.is_empty() => {
                Some(Command::PickerInput(c.to_string()))
            }
            _ => None,
        }
    }

    fn help(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') => Some(Command::CloseHelp),
            KeyCode::Char('q') => Some(Command::Quit),
            _ => None,
        }
    }

    /// REQ-004 IMAGEVIEW 级键位（D-14；Notes/05 §10 状态栏）：
    /// `o` 系统查看器 / `y` 复制路径 / `q` 关闭。
    fn image_view(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Char('o') if key.modifiers.is_empty() => Some(Command::ImageViewOpenExternal),
            KeyCode::Char('y') if key.modifiers.is_empty() => Some(Command::ImageViewCopy),
            KeyCode::Char('q') if key.modifiers.is_empty() => Some(Command::ImageViewClose),
            _ => None,
        }
    }

    /// REQ-007 @ 提及候选键位（AC-007-23）：普通字符进候选 query（本地即时
    /// 过滤 + 两源异步拉取）、j/k/↑↓ 移动命中、Enter 回填、Esc/q 关闭回
    /// INSERT、Backspace 删候选 query 字符。
    fn mention(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Char('q') if key.modifiers.is_empty() => Some(Command::ClosePicker),
            KeyCode::Enter if key.modifiers.is_empty() => Some(Command::PickerConfirm),
            KeyCode::Up => Some(Command::PickerUp),
            KeyCode::Down => Some(Command::PickerDown),
            KeyCode::Char('k') if key.modifiers.is_empty() => Some(Command::PickerUp),
            KeyCode::Char('j') if key.modifiers.is_empty() => Some(Command::PickerDown),
            KeyCode::Backspace => Some(Command::PickerBackspace),
            KeyCode::Char(c) if key.modifiers.is_empty() => {
                Some(Command::PickerInput(c.to_string()))
            }
            _ => None,
        }
    }

    /// REQ-007 subagent 目录键位（AC-007-07~10）：j/k 移动、Enter 展开/折叠
    /// （has_children 拉取子目录）、x 中断（二次确认在 reducer）、Esc/q 关闭。
    fn subagent(&mut self, key: KeyEvent) -> Option<Command> {
        self.pending_g = false;
        match key.code {
            KeyCode::Esc => Some(Command::ClosePicker),
            KeyCode::Char('q') if key.modifiers.is_empty() => Some(Command::ClosePicker),
            KeyCode::Enter if key.modifiers.is_empty() => Some(Command::PickerConfirm),
            KeyCode::Char('j') if key.modifiers.is_empty() => Some(Command::PickerDown),
            KeyCode::Char('k') if key.modifiers.is_empty() => Some(Command::PickerUp),
            KeyCode::Char('x') if key.modifiers.is_empty() => Some(Command::SubagentInterrupt),
            _ => None,
        }
    }

    /// REQ-009 MONITOR 键位（FR-009-04，vim 风格）：`j/k` 焦点、`gg/G` 首尾、
    /// `Enter` 详情、`c` 问答、`f` 加油、`l` 定位、`s` KB 统计、`/` 过滤、
    /// `q` 退出、`?` 帮助。Chat/Filter 输入态复用 Insert 语义（Esc/Enter/
    /// Backspace/字符）。
    fn monitor(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('g') if !ctrl => {
                if self.pending_g {
                    self.pending_g = false;
                    Some(Command::GotoTop)
                } else {
                    self.pending_g = true;
                    None
                }
            }
            KeyCode::Char(c) => {
                self.pending_g = false;
                match (c, ctrl) {
                    ('j', false) => Some(Command::MoveDown),
                    ('k', false) => Some(Command::MoveUp),
                    ('G', false) => Some(Command::GotoBottom),
                    ('c', false) => Some(Command::MonitorOpenChat),
                    ('f', false) => Some(Command::MonitorCheer),
                    ('l', false) => Some(Command::MonitorLocate),
                    ('s', false) => Some(Command::MonitorStats),
                    ('/', false) => Some(Command::StartSearch),
                    ('?', false) => Some(Command::OpenHelp),
                    ('q', false) => Some(Command::Quit),
                    ('c', true) => Some(Command::Quit),
                    _ => None,
                }
            }
            KeyCode::Enter if !ctrl => {
                self.pending_g = false;
                Some(Command::OpenFocused)
            }
            KeyCode::Esc => {
                self.pending_g = false;
                Some(Command::ClosePicker)
            }
            _ => {
                self.pending_g = false;
                None
            }
        }
    }
}

// ============================================================================
// REQ-007 AC-007-21：`[keymap]` 覆盖（Step 5；设计 A 最小接口 re-home）。
//
// 机制（经 keymap_override_proto 原型验证 PASS）：
// - Keymap 持有每个 mode 的「默认单键表」与「生效表（默认 ∘ 覆盖）」；
// - `KeyDecoder` 无覆盖（默认 new()）时 decode 与既有硬编码分支 **逐位一致**
//   （no-op 回落，既有 ~54 keymap 测试不改仍绿是提取正确性护栏）；
// - 有覆盖时 decode_key 顶部查 override 层：新键命中即返回、原默认键被
//   shadow（无需 post-decode remap 表达了解绑/改键）；未知 command/非法
//   keyspec/位移冲突 → warn 保默认（非失败合并）。
// 覆盖范围 = 无字符输入 catch-all 的命令型 mode（Normal/Trajectory/Visual/
// Approval/ApprovalList/ImageView/Help/Monitor）；输入型 mode（Search/
// ModelCatalog/CommandPalette/Picker/Insert…）字符兼作输入，不改键。
// `g` 前缀键保留（双键 gg/gt/gT/gv 结构性命令不可重绑，原型已验证）。
// ============================================================================

/// 可重绑命令的规范名（配置键）。每个 mode 的表引用同名条，命令本身
/// mode 相关（同名字符在 Normal 是 MoveDown、在 Picker 是 PickerDown——
/// 但输入型 mode 不在此表）。
fn cmd_display(c: &Command) -> String {
    format!("{c:?}")
}

/// 默认单键表提取（逐行对应 `KeyDecoder::normal`/`trajectory`/… 硬编码
/// 分支；无字符输入 catch-all 的 mode）。`name` 为 `[keymap.modes.<mode>]`
/// 配置键。payload 型命令（VisualStart/OpenSession/PickerInput 等）不入表
/// （改键语义依赖 payload，非目标）。
fn default_key_tables() -> std::collections::BTreeMap<InputMode, Vec<(&'static str, char, Command)>>
{
    use Command::*;
    let mut m = std::collections::BTreeMap::new();
    m.insert(
        InputMode::Normal,
        vec![
            ("move_down", 'j', MoveDown),
            ("move_up", 'k', MoveUp),
            ("goto_bottom", 'G', GotoBottom),
            ("open_picker", 'f', OpenPicker),
            ("insert_mode", 'i', InsertMode),
            ("open_help", '?', OpenHelp),
            ("quit", 'q', Quit),
            ("retry_probe", 'r', RetryProbe),
            ("stop_running", 's', StopRunning),
            ("collapse_project", 'h', CollapseProject),
            ("expand_project", 'l', ExpandProject),
            ("open_selected", 'o', OpenSelected),
            ("yank_context", 'y', YankContext),
            ("open_outline", 'O', OpenOutline),
            ("next_turn", ']', NextTurn),
            ("prev_turn", '[', PrevTurn),
            ("goto_chat", '1', GotoChat),
            ("toggle_trajectory", '2', ToggleTrajectory),
            ("open_model_catalog", 'M', OpenModelCatalog),
            ("open_command_palette", ':', OpenCommandPalette),
        ],
    );
    m.insert(
        InputMode::Trajectory,
        vec![
            ("move_down", 'j', MoveDown),
            ("move_up", 'k', MoveUp),
            ("toggle_fold", 'z', ToggleFold),
            ("open_detail", 'd', OpenDetail),
            ("yank_context", 'y', YankContext),
            ("quit", 'q', Quit),
            ("goto_bottom", 'G', GotoBottom),
        ],
    );
    m.insert(
        InputMode::Visual,
        vec![
            ("move_down", 'j', MoveDown),
            ("move_up", 'k', MoveUp),
            ("yank_context", 'y', YankContext),
            ("open_selected", 'o', OpenSelected),
        ],
    );
    m.insert(
        InputMode::Approval,
        vec![
            ("approval_allow", 'y', ApprovalAllow),
            ("approval_reject", 'n', ApprovalReject),
            ("approval_cancel", 'q', ApprovalCancel),
            ("approval_always", 'a', ApprovalAlways),
            ("open_approval_list", 'L', OpenApprovalList),
        ],
    );
    m.insert(
        InputMode::ApprovalList,
        vec![
            ("approval_retry", 'r', ApprovalRetry),
            ("approval_batch_allow", 'A', ApprovalBatchAllow),
            ("approval_allow", 'y', ApprovalAllow),
            ("approval_reject", 'n', ApprovalReject),
            ("approval_always", 'a', ApprovalAlways),
        ],
    );
    m.insert(
        InputMode::ImageView,
        vec![
            ("image_view_open_external", 'o', ImageViewOpenExternal),
            ("image_view_copy", 'y', ImageViewCopy),
            ("image_view_close", 'q', ImageViewClose),
        ],
    );
    m.insert(
        InputMode::Help,
        vec![("quit", 'q', Quit), ("close_help", '?', CloseHelp)],
    );
    m.insert(
        InputMode::Monitor,
        vec![
            ("move_down", 'j', MoveDown),
            ("move_up", 'k', MoveUp),
            ("goto_bottom", 'G', GotoBottom),
            ("monitor_open_chat", 'c', MonitorOpenChat),
            ("monitor_cheer", 'f', MonitorCheer),
            ("monitor_locate", 'l', MonitorLocate),
            ("monitor_stats", 's', MonitorStats),
            ("quit", 'q', Quit),
        ],
    );
    m
}

/// Effective keymap（默认 ∘ 覆盖）。每 mode：`defaults`（原始）与
/// `effective`（覆盖后）。
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    defaults: std::collections::BTreeMap<InputMode, std::collections::BTreeMap<char, Command>>,
    effective: std::collections::BTreeMap<InputMode, std::collections::BTreeMap<char, Command>>,
    /// build 期的可读警告（未知 command/mode、非法 keyspec、位移冲突）。
    pub warnings: Vec<String>,
}

impl Keymap {
    /// 从 `[keymap]` 配置非失败合并。未知项 warn 保默认，永不崩溃。
    pub fn build(cfg: &crate::config::KeymapConfig) -> Self {
        let default_rows = default_key_tables();
        let mut defaults = std::collections::BTreeMap::new();
        let mut effective = std::collections::BTreeMap::new();
        for (mode, rows) in &default_rows {
            let def: std::collections::BTreeMap<char, Command> =
                rows.iter().map(|(_, k, c)| (*k, c.clone())).collect();
            defaults.insert(*mode, def.clone());
            effective.insert(*mode, def);
        }
        let mut warnings = Vec::new();
        for (mode_name, binds) in &cfg.modes {
            let Some(mode) = mode_from_name(mode_name) else {
                warnings.push(format!("未知 mode: {mode_name:?}（忽略）"));
                continue;
            };
            let Some(rows) = default_rows.get(&mode) else {
                warnings.push(format!("mode {mode_name:?} 为输入型/不支持覆盖（忽略）"));
                continue;
            };
            let def = defaults.get(&mode).cloned().unwrap_or_default();
            let eff = effective.entry(mode).or_default();
            for (cmd_name, keyspec) in binds {
                let Some((_old_key, _, default_cmd)) =
                    rows.iter().find(|(name, _, _)| name == cmd_name)
                else {
                    warnings.push(format!("{mode_name}.{cmd_name}: 未知 command（保留默认）"));
                    continue;
                };
                // 定位该 command 的默认键。
                let default_key = def
                    .iter()
                    .find_map(|(k, c)| (c == default_cmd).then_some(*k));
                if keyspec == "none" {
                    if let Some(k) = default_key {
                        eff.remove(&k);
                    }
                    continue;
                }
                // 单字符 plain key（原型已验证范围；双键/修饰键暂 warn）。
                let mut cs = keyspec.chars();
                let (Some(c), None) = (cs.next(), cs.next()) else {
                    warnings.push(format!(
                        "{mode_name}.{cmd_name}={keyspec:?}: 非法键序列（本版支持单字符或 none，保留默认）"
                    ));
                    continue;
                };
                if !c.is_ascii() || c.is_whitespace() || c.is_control() {
                    warnings.push(format!(
                        "{mode_name}.{cmd_name}={keyspec:?}: 非法键序列（保留默认）"
                    ));
                    continue;
                }
                if c == 'g' {
                    warnings.push(format!(
                        "{mode_name}.{cmd_name}: g 为前缀键不可重绑（保留默认）"
                    ));
                    continue;
                }
                // 原默认键失绑。
                if let Some(k) = default_key {
                    eff.remove(&k);
                }
                // 后写胜；位移既有默认绑定 → warn。
                if let Some(displaced) = defaults.get(&mode).and_then(|d| d.get(&c)) {
                    if displaced != default_cmd {
                        warnings.push(format!(
                            "{mode_name}.{cmd_name}={c} 位移默认绑定 {}（{}）",
                            cmd_display(displaced),
                            c
                        ));
                    }
                }
                eff.insert(c, default_cmd.clone());
            }
        }
        Keymap {
            defaults,
            effective,
            warnings,
        }
    }

    /// 生效差异行（帮助面板联动/`--dump-keymap` 单一事实源）：`mode 键 →
    /// 命令` 或 `mode 键 unbind`（仅列与默认不同的项）。
    pub fn override_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (mode, eff) in &self.effective {
            let def = self.defaults.get(mode).cloned().unwrap_or_default();
            let mode_name = mode_name_of(*mode);
            for (k, cmd) in eff {
                let old = def.get(k);
                if old != Some(cmd) {
                    out.push(format!(
                        "[{mode_name}] {k} → {}（默认 {}）",
                        cmd_display(cmd),
                        old.map(cmd_display).unwrap_or_else(|| "无".into())
                    ));
                }
            }
            for (k, cmd) in &def {
                if !eff.contains_key(k) {
                    out.push(format!("[{mode_name}] {k} unbind（{}）", cmd_display(cmd)));
                }
            }
        }
        out
    }

    /// 顶部查表（仅 plain 无修饰单字符）：命中 → Some(Some(cmd))；原默认键被
    /// shadow → Some(None)；未命中（含结构性键/未覆盖默认键）→ None 回落既有
    /// 硬编码分支。
    pub(crate) fn decode_override(
        &self,
        mode: InputMode,
        key: KeyEvent,
    ) -> Option<Option<Command>> {
        let KeyCode::Char(c) = key.code else {
            return None;
        };
        if key.modifiers != KeyModifiers::NONE {
            return None;
        }
        let eff = self.effective.get(&mode).and_then(|t| t.get(&c)).cloned();
        let def = self.defaults.get(&mode).and_then(|t| t.get(&c)).cloned();
        match (eff, def) {
            // 新键/改键命中（override 层拥有该键）。
            (Some(cmd), Some(old)) if cmd != old => Some(Some(cmd)),
            (Some(cmd), None) => Some(Some(cmd)),
            // 原默认键：command 被改键/解绑 → shadow（返回 None，不改语义）。
            (None, Some(_)) => Some(None),
            // 未覆盖默认键 → 回落生产分支。
            _ => None,
        }
    }
}

fn mode_name_of(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Normal => "normal",
        InputMode::Trajectory => "trajectory",
        InputMode::Visual => "visual",
        InputMode::Approval => "approval",
        InputMode::ApprovalList => "approval_list",
        InputMode::ImageView => "image_view",
        InputMode::Help => "help",
        InputMode::Monitor => "monitor",
        InputMode::Picker
        | InputMode::Insert
        | InputMode::Search
        | InputMode::TrajectoryFilter
        | InputMode::ModelCatalog
        | InputMode::CommandPalette
        | InputMode::Mention => "input(不可覆盖)",
        InputMode::Subagent => "subagent",
    }
}

fn mode_from_name(name: &str) -> Option<InputMode> {
    match name {
        "normal" => Some(InputMode::Normal),
        "trajectory" => Some(InputMode::Trajectory),
        "visual" => Some(InputMode::Visual),
        "approval" => Some(InputMode::Approval),
        "approval_list" => Some(InputMode::ApprovalList),
        "image_view" => Some(InputMode::ImageView),
        "help" => Some(InputMode::Help),
        "monitor" => Some(InputMode::Monitor),
        _ => None,
    }
}

/// Stateless convenience wrapper for one event.
pub fn map_key(mode: InputMode, event: Event) -> Option<Command> {
    let mut decoder = KeyDecoder::new();
    decoder.decode(mode, event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    #[test]
    fn normal_vim_navigation_and_global_keys() {
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('j'))),
            Some(Command::MoveDown)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('k'))),
            Some(Command::MoveUp)
        );
        assert_eq!(
            d.decode(InputMode::Normal, ctrl('d')),
            Some(Command::HalfPageDown)
        );
        assert_eq!(
            d.decode(InputMode::Normal, ctrl('u')),
            Some(Command::HalfPageUp)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('G'))),
            Some(Command::GotoBottom)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('f'))),
            Some(Command::OpenPicker)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('?'))),
            Some(Command::OpenHelp)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('q'))),
            Some(Command::Quit)
        );
        assert_eq!(d.decode(InputMode::Normal, ctrl('c')), Some(Command::Quit));
        assert_eq!(
            d.decode(InputMode::Normal, ctrl('w')),
            Some(Command::CycleFocus)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('s'))),
            Some(Command::StopRunning)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('h'))),
            Some(Command::CollapseProject)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('l'))),
            Some(Command::ExpandProject)
        );
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('o'))),
            Some(Command::OpenSelected)
        );
        // REQ-006：`M` 打开模型目录（FR-006-01）。
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('M'))),
            Some(Command::OpenModelCatalog)
        );
    }

    #[test]
    fn model_catalog_mode_keys_filter_navigate_and_close_ac006() {
        let mut d = KeyDecoder::new();
        // 字符进查询（本地即时过滤）。
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Char('v'))),
            Some(Command::PickerInput("v".into()))
        );
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Char('4'))),
            Some(Command::PickerInput("4".into()))
        );
        // j/k 移动、Enter 确认、Backspace 删、Esc/q 关闭。
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Char('j'))),
            Some(Command::PickerDown)
        );
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Char('k'))),
            Some(Command::PickerUp)
        );
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Enter)),
            Some(Command::PickerConfirm)
        );
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Backspace)),
            Some(Command::PickerBackspace)
        );
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Esc)),
            Some(Command::ClosePicker)
        );
        assert_eq!(
            d.decode(InputMode::ModelCatalog, key(KeyCode::Char('q'))),
            Some(Command::ClosePicker),
            "q=关闭（与 Esc 同义）"
        );
    }

    #[test]
    fn gg_is_a_two_key_goto_top_command() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('g'))),
            Some(Command::GotoTop)
        );
        assert_eq!(d.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('j'))),
            Some(Command::MoveDown)
        );
    }

    #[test]
    fn picker_accepts_text_and_navigation() {
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.decode(InputMode::Picker, key(KeyCode::Char('a'))),
            Some(Command::PickerInput("a".into()))
        );
        assert_eq!(
            d.decode(InputMode::Picker, key(KeyCode::Down)),
            Some(Command::PickerDown)
        );
        assert_eq!(
            d.decode(InputMode::Picker, key(KeyCode::Up)),
            Some(Command::PickerUp)
        );
        assert_eq!(
            d.decode(InputMode::Picker, key(KeyCode::Backspace)),
            Some(Command::PickerBackspace)
        );
        assert_eq!(
            d.decode(InputMode::Picker, key(KeyCode::Enter)),
            Some(Command::PickerConfirm)
        );
        assert_eq!(
            d.decode(InputMode::Picker, key(KeyCode::Esc)),
            Some(Command::ClosePicker)
        );
        assert_eq!(
            d.decode(InputMode::Insert, key(KeyCode::Enter)),
            Some(Command::SubmitInput)
        );
        assert_eq!(
            d.decode(InputMode::Insert, key(KeyCode::Esc)),
            Some(Command::ClosePicker)
        );
    }

    #[test]
    fn approval_list_keys_navigate_retry_batch_and_close_ac006() {
        let mut d = KeyDecoder::new();
        // 单条槽：L 打开列表。
        assert_eq!(
            d.decode(InputMode::Approval, key(KeyCode::Char('L'))),
            Some(Command::OpenApprovalList)
        );
        // 列表：j/k 移动、r 重试、A 批量、y/n 单条决策、q/Esc/L 回单条槽。
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('j'))),
            Some(Command::PickerDown)
        );
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('k'))),
            Some(Command::PickerUp)
        );
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('r'))),
            Some(Command::ApprovalRetry)
        );
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('A'))),
            Some(Command::ApprovalBatchAllow)
        );
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('q'))),
            Some(Command::CloseApprovalList)
        );
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Esc)),
            Some(Command::CloseApprovalList)
        );
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('y'))),
            Some(Command::ApprovalAllow)
        );
        assert_eq!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('n'))),
            Some(Command::ApprovalReject)
        );
        // 列表中 `q` 不回 Quit（不中止当前项）。
        assert_ne!(
            d.decode(InputMode::ApprovalList, key(KeyCode::Char('q'))),
            Some(Command::Quit)
        );
    }

    #[test]
    fn approval_single_slot_still_decides_with_y_n_q_a_ac003() {
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.decode(InputMode::Approval, key(KeyCode::Char('y'))),
            Some(Command::ApprovalAllow)
        );
        assert_eq!(
            d.decode(InputMode::Approval, key(KeyCode::Char('n'))),
            Some(Command::ApprovalReject)
        );
        assert_eq!(
            d.decode(InputMode::Approval, key(KeyCode::Char('q'))),
            Some(Command::ApprovalCancel)
        );
        assert_eq!(
            d.decode(InputMode::Approval, key(KeyCode::Esc)),
            Some(Command::ApprovalCancel)
        );
        assert_eq!(
            d.decode(InputMode::Approval, key(KeyCode::Char('a'))),
            Some(Command::ApprovalAlways)
        );
    }

    #[test]
    fn resize_and_key_release_are_handled() {
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.decode(InputMode::Normal, Event::Resize(120, 40)),
            Some(Command::Resize {
                width: 120,
                height: 40
            })
        );
        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert_eq!(d.decode(InputMode::Normal, release), None);
    }

    #[test]
    fn insert_enter_ctrl_or_alt_is_newline_plain_is_submit() {
        // REQ-002 FR-002-01：Ctrl+Enter / Alt+Enter 换行；裸 Enter 发送。
        let mut d = KeyDecoder::new();
        let ctrl_enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(
            d.decode(InputMode::Insert, ctrl_enter),
            Some(Command::PickerInput("\n".into()))
        );
        let alt_enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(
            d.decode(InputMode::Insert, alt_enter),
            Some(Command::PickerInput("\n".into()))
        );
        let plain_enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            d.decode(InputMode::Insert, plain_enter),
            Some(Command::SubmitInput)
        );
        // Shift+Enter 等其它 modifier 组合不当作发送（V0.1 保守处理）。
        let shift_enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_ne!(
            d.decode(InputMode::Insert, shift_enter),
            Some(Command::SubmitInput)
        );
        // Ctrl+c keeps the global quit path while composing (AC-002-07).
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(
            d.decode(InputMode::Insert, ctrl_c),
            Some(Command::Quit),
            "INSERT 中 Ctrl+c → Quit"
        );
    }
}

#[cfg(test)]
mod image_view_tests {
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn image_view_keys_map_to_viewer_copy_close() {
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.decode(InputMode::ImageView, key(KeyCode::Char('o'))),
            Some(Command::ImageViewOpenExternal)
        );
        assert_eq!(
            d.decode(InputMode::ImageView, key(KeyCode::Char('y'))),
            Some(Command::ImageViewCopy)
        );
        assert_eq!(
            d.decode(InputMode::ImageView, key(KeyCode::Char('q'))),
            Some(Command::ImageViewClose)
        );
        // 其余键不产生命令（不误触）。
        assert_eq!(
            d.decode(InputMode::ImageView, key(KeyCode::Char('j'))),
            None
        );
        assert_eq!(d.decode(InputMode::ImageView, key(KeyCode::Enter)), None);
    }

    #[test]
    fn normal_enter_emits_open_focused() {
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Enter)),
            Some(Command::OpenFocused)
        );
        // o 保持原有 OpenSelected 语义。
        assert_eq!(
            d.decode(InputMode::Normal, key(KeyCode::Char('o'))),
            Some(Command::OpenSelected)
        );
    }
}
