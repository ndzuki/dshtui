//! AppState、事件编排与帧循环骨架（Notes/02 §3/§4；Step 4 是唯一维护
//! mpsc 顺序的地方）。
//!
//! 设计要点（Step 4 Prototype 验证结论，`examples/proto_step4.rs`）：
//! - 单一 reducer：`handle(event) -> Vec<Cmd>`，UI/transport 绝不直接持有模型可变引用；
//! - page 请求**单飞**（`in_flight`）+ **generation** 防 stale response（乱序到达直接丢弃）；
//! - **断线/reconnecting 期间禁止发 HTTP page**——只记 `want_backfill`，
//!   refollow snapshot 完成后补发（AC-001-12 重连-分页对账）；
//! - 任何成功的服务器响应都置 `Ready`；断线置 `Reconnecting`（状态条可见，不静默吞事件）；
//! - running 会话退出命令顺序：cancel → terminal restore → exit（AC-001-08）；
//! - 权限错误（PERMISSION_DENIED）不自动重试（Notes/03 §8）。

use std::collections::HashSet;

use crate::api::types::ChunkRow;
use crate::api::types::{
    ListItemRaw, SessionHistoryRecord, SessionId, SessionLogOffset, SessionSeq, SessionWireEvent,
};
use crate::api::{ClientError, ErrorClass};
use crate::model::{ApplyEffect, Incoming, SessionStore, WorkspaceStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Picker,
    /// 预留（REQ-002 composer 扩展点）。
    Insert,
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
    /// 启动探测/认证失败：展示「请启动 dsh web / 检查 127.0.0.1:3080」指引（AC-001-02）。
    StartupFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewport {
    /// 视口顶部在窗口块列表中的偏移（块索引）。
    pub offset: usize,
    /// 是否贴住 live tail（流式跟随；用户向上滚动时冻结）。
    pub follow_tail: bool,
    /// 可视高度（行）。
    pub height: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            offset: 0,
            follow_tail: true,
            height: 24,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PickerState {
    pub open: bool,
    pub query: String,
    pub selection: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageGuard {
    pub generation: u64,
    pub in_flight: bool,
}

/// 进入 AppState 的事件（来自 api 任务 / 输入层 / 帧循环）。
#[derive(Debug)]
pub enum AppEvent {
    /// 启动：认证成功后先拉会话列表。
    Startup,
    SessionListPage {
        items: Vec<ListItemRaw>,
        next_cursor: Option<String>,
    },
    SessionListError(ClientError),
    /// workspace/follow 原始帧（api 层已容忍未知形态）。
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
    /// 启动探测失败（AC-001-02 指引）。
    StartupProbeFailed(String),
    Resize {
        width: u16,
        height: u16,
    },
}

/// reducer 输出的编排命令（由 run 循环执行）。
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
    RestoreTerminal,
    Exit,
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
    pub help_open: bool,
    pub quit_requested: bool,
    pub exited: bool,
    /// 面向用户的最近错误（状态条/指引区展示，不刷屏）。
    pub last_error: Option<String>,
    /// 启动指引（AC-001-02）。
    pub startup_guidance: Option<String>,
    /// 终端尺寸（渲染断点输入）。
    pub width: u16,
    pub height: u16,
    /// 窗口消息上限（配置）。
    pub window_cap: usize,
    page_guard: PageGuard,
    want_backfill: bool,
    running_sessions: HashSet<SessionId>,
    list_cursor: Option<String>,
    list_loaded: bool,
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
            help_open: false,
            quit_requested: false,
            exited: false,
            last_error: None,
            startup_guidance: None,
            width: 80,
            height: 24,
            window_cap: 200,
            page_guard: PageGuard::default(),
            want_backfill: false,
            running_sessions: HashSet::new(),
            list_cursor: None,
            list_loaded: false,
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

    // ---------- 查询（UI 只读） ----------

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

    /// AC-001-02 指引文本（探针失败时展示；含重试/退出提示）。
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
                    // 继续分页直到列表加载完成（本地增量合并，Notes/06 §1）。
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
                if let Some(items) = crate::api::workspace::extract_workspaces(&frame) {
                    for item in items {
                        let id = item
                            .get("id")
                            .or_else(|| item.get("workspaceId"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if id.is_empty() {
                            continue;
                        }
                        let title = item.get("title").and_then(|v| v.as_str()).map(String::from);
                        let wid = crate::api::types::WorkspaceId(id);
                        self.workspaces.upsert_workspace(wid.clone(), title);
                        // Session membership: attach to the workspace this frame
                        // item describes (never "last upserted" — frame item
                        // order is server-owned).
                        if let Some(sids) = item
                            .get("sessionIds")
                            .or_else(|| item.get("sessions"))
                            .and_then(|v| v.as_array())
                        {
                            for s in sids.iter().filter_map(|s| s.as_str()) {
                                self.workspaces
                                    .attach_session_to_workspace(&wid, &SessionId(s.to_string()));
                            }
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
                // 任何成功的服务器响应都证明连接就绪。
                self.conn = ConnState::Ready;
                self.active_session = Some(session_id.clone());
                // running 来自官方 projections（不自算）。
                let running = projections
                    .as_ref()
                    .and_then(|p| p.get("running"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if running {
                    self.running_sessions.insert(session_id.clone());
                } else {
                    self.running_sessions.remove(&session_id);
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
                // 重连对账：refollow 完成后补发浏览缺口（AC-001-12）。
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
                vec![]
            }
            AppEvent::FollowChunks { session_id, row } => {
                let Some(w) = self.sessions.get_mut(&session_id.0) else {
                    return vec![];
                };
                let eff = w.apply(Incoming::Chunks(row));
                self.adjust_viewport(&eff);
                vec![]
            }
            AppEvent::FollowError { session_id, error } => {
                if error.class() == ErrorClass::PermissionDenied {
                    // 权限错误不自动重试（Notes/03 §8），也不断连。
                    self.last_error = Some(format!("权限不足（{}）：{}", session_id, error));
                    vec![]
                } else {
                    // 其它流错误按断开处理 → 统一重连编排。
                    self.handle_disconnected(format!("follow 流错误: {error}"))
                }
            }
            AppEvent::PageResult {
                session_id,
                generation,
                records,
                has_more,
            } => {
                // generation 校验：stale response 直接丢弃（单飞 + 防乱序）。
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
            AppEvent::Disconnected(reason) => self.handle_disconnected(reason),
            AppEvent::Reconnected => {
                self.conn = ConnState::Ready;
                // 恢复后只触发一次 refollow（不重复 repair）。
                match self.active_session.clone() {
                    Some(sid) => vec![Cmd::OpenFollow {
                        session_id: sid,
                        max_messages: self.window_cap,
                    }],
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
                vec![]
            }
        }
    }

    fn handle_disconnected(&mut self, reason: String) -> Vec<Cmd> {
        self.conn = ConnState::Reconnecting;
        // 在途 page 作废（generation 保留，防 stale 复燃）。
        self.page_guard.in_flight = false;
        tracing::warn!(%reason, "连接断开 → reconnecting");
        vec![Cmd::Reconnect { delay_ms: 500 }]
    }

    fn page_cmd(&mut self) -> Cmd {
        self.page_guard.generation += 1;
        self.page_guard.in_flight = true;
        let session_id = self.active_session.clone().expect("page_cmd 需要活动会话");
        // throughSeq = follow 快照 cursor；beforeSeq = 窗口最旧 seq。
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

    /// ApplyEffect → 视口平移（滚动不抖的关键）。
    fn adjust_viewport(&mut self, eff: &ApplyEffect) {
        match eff {
            ApplyEffect::Rebuilt => {
                // 整窗重建：贴尾。
                self.viewport.follow_tail = true;
                self.scroll_to_bottom();
            }
            ApplyEffect::TailAppended { anchor_stable, .. } => {
                if self.viewport.follow_tail {
                    self.scroll_to_bottom();
                } else if !*anchor_stable {
                    // 头部被逐出：视口同步前移，保持浏览相对位置。
                    self.viewport.offset = self.viewport.offset.saturating_sub(1);
                }
                // anchor_stable 且浏览中：不动（不抖）。
            }
            ApplyEffect::HeadPrepend { anchor_shift, .. } => {
                // 前插：视口平移 anchor_shift，浏览位置不跳。
                self.viewport.offset += anchor_shift;
            }
            ApplyEffect::Noop => {}
        }
    }

    fn scroll_to_bottom(&mut self) {
        let len = self.active_window().map(|w| w.len()).unwrap_or(0);
        let height = self.viewport.height.max(1);
        self.viewport.offset = len.saturating_sub(height);
    }

    fn record_error(&mut self, e: ClientError, prefix: &str) {
        // 权限/业务错误提示用户；网络错误只进日志 + 状态条（不刷屏）。
        let msg = format!("{prefix}: {e}");
        match e.class() {
            ErrorClass::Retryable => tracing::warn!(error = %msg, "可重试错误"),
            _ => {
                tracing::error!(error = %msg, "错误");
                self.last_error = Some(msg);
            }
        }
    }

    // ---------- 输入命令（Step 5 keymap 映射到此处） ----------

    pub fn handle_command(&mut self, cmd: crate::input::Command) -> Vec<Cmd> {
        use crate::input::Command as C;
        match cmd {
            C::MoveDown
            | C::MoveUp
            | C::HalfPageDown
            | C::HalfPageUp
            | C::GotoBottom
            | C::GotoTop => self.scroll(cmd),
            C::OpenPicker => {
                self.mode = Mode::Picker;
                self.picker.open = true;
                self.picker.query.clear();
                self.picker.selection = 0;
                vec![]
            }
            C::InsertMode => {
                self.mode = Mode::Insert;
                vec![]
            }
            C::ClosePicker => {
                self.mode = Mode::Normal;
                self.picker.open = false;
                vec![]
            }
            C::PickerDown => {
                self.picker.selection += 1;
                vec![]
            }
            C::PickerUp => {
                self.picker.selection = self.picker.selection.saturating_sub(1);
                vec![]
            }
            C::PickerInput(text) => {
                self.picker.query.push_str(&text);
                self.picker.selection = 0;
                vec![]
            }
            C::PickerBackspace => {
                self.picker.query.pop();
                vec![]
            }
            C::SubmitInput => {
                self.mode = Mode::Normal;
                vec![]
            }
            C::OpenSelected => self.open_session_from_selection(),
            C::StopRunning => self
                .active_session
                .clone()
                .filter(|sid| self.running_sessions.contains(sid))
                .map(|sid| vec![Cmd::CancelSession(sid)])
                .unwrap_or_default(),
            C::CollapseProject | C::ExpandProject => vec![],
            C::PickerConfirm => {
                let chosen = self.picker_selected_session();
                self.mode = Mode::Normal;
                self.picker.open = false;
                match chosen {
                    Some(sid) => self.open_session(sid),
                    None => vec![],
                }
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
            C::Quit => self.quit(),
            C::RetryProbe => {
                self.conn = ConnState::Connecting;
                self.startup_guidance = None;
                vec![]
            }
            C::Resize { width, height } => self.handle(AppEvent::Resize { width, height }),
        }
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
        cmds
    }

    /// 顶部且还有更早历史 → 发 page（单飞 + 仅 Ready + 断线只记 want_backfill）。
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
        self.active_session = Some(sid.clone());
        self.viewport.follow_tail = true;
        vec![Cmd::OpenFollow {
            session_id: sid,
            max_messages: self.window_cap,
        }]
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

    /// AC-001-08：运行中先 stop（cancel），再恢复终端，最后退出。
    pub fn quit(&mut self) -> Vec<Cmd> {
        self.quit_requested = true;
        let mut cmds = Vec::new();
        if let Some(sid) = self.active_session.clone() {
            if self.running_sessions.contains(&sid) {
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
    fn startup_probe_failed_shows_guidance_ac001_02() {
        let mut s = AppState::default();
        let cmds = s.handle(AppEvent::StartupProbeFailed("连接被拒绝".into()));
        assert!(cmds.is_empty(), "探针失败不拉起后端进程");
        assert_eq!(s.conn, ConnState::StartupFailed);
        let g = s.guidance_text();
        assert!(g.contains("请启动 dsh web"), "guidance={g}");
        assert!(g.contains("127.0.0.1:3080"), "guidance={g}");
        assert!(g.contains("[r] 重试"), "guidance={g}");
        // 重试路径：RetryProbe 回到 Connecting 且清空指引（恢复不被旧状态污染）。
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
        // 在途期间再次到顶 → 不发新请求。
        assert!(s.handle_command(C::GotoTop).is_empty());
        // stale 响应丢弃。
        let cmds = s.handle(AppEvent::PageResult {
            session_id: SessionId("s1".into()),
            generation: gen + 99,
            records: vec![],
            has_more: None,
        });
        assert!(cmds.is_empty());
        // 正常完成释放单飞。
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
        // 断线期间滚动不发 HTTP page。
        let cmds = s.handle_command(C::GotoTop);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::RequestPage { .. })));
        // 恢复 → 只触发一次 refollow。
        let cmds = s.handle(AppEvent::Reconnected);
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::OpenFollow { .. }))
                .count(),
            1
        );
        assert_eq!(s.conn, ConnState::Ready);
        // refollow snapshot 到达 → 补发 backfill（AC-001-12 对账）。
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
        let cmds = s.handle_command(C::Quit);
        assert_eq!(
            cmds,
            vec![
                Cmd::CancelSession(SessionId("s1".into())),
                Cmd::RestoreTerminal,
                Cmd::Exit,
            ]
        );
        // 非 running：无 cancel。
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
        // follow_tail：追加事件保持贴尾。
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
        // 向上滚冻结 tail。
        s.handle_command(C::GotoTop);
        assert!(!s.viewport.follow_tail);
        assert_eq!(s.viewport.offset, 0);
        // 追加事件不动浏览位置（anchor stable）。
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
        // 旧 generation 的错误不影响当前单飞。
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
}
