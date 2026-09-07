//! Vim-like keymap for the V0.1 input contract (`Notes/04-ui-ux-design.md §3`).
//!
//! The decoder is deliberately pure: it consumes crossterm events and emits
//! domain commands; it never mutates AppState or talks to the transport.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Input modes that affect key meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    /// REQ-009 V0.3：MONITOR 模式（`dshtui monitor` 独立键位表）。
    Monitor,
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
}

/// Stateful decoder for multi-key Normal-mode commands such as `gg`.
#[derive(Debug, Clone, Default)]
pub struct KeyDecoder {
    pending_g: bool,
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.pending_g = false;
    }

    /// Translate one crossterm event. Key releases and unsupported events are ignored.
    pub fn decode(&mut self, mode: InputMode, event: Event) -> Option<Command> {
        match event {
            Event::Resize(width, height) => Some(Command::Resize { width, height }),
            Event::Key(key) if key.kind == KeyEventKind::Press => self.decode_key(mode, key),
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
            InputMode::Monitor => self.monitor(key),
        }
    }

    /// `g` 前缀在按下 g 时置位；第二键 t/T/gg 触发 Tab/顶部命令，其它键清
    /// 前缀后按普通单键解码（g 后 j → MoveDown，回归语义）。
    fn g_prefix_key(&mut self, c: char) -> Option<Command> {
        match c {
            't' => Some(Command::ToggleTrajectory),
            'T' => Some(Command::GotoChat),
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
