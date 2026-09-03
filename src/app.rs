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
    /// Reserved (REQ-002 composer extension point).
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
    /// Most recent user-facing error (shown in the status bar/guidance area,
    /// no spam).
    pub last_error: Option<String>,
    /// Startup guidance (AC-001-02).
    pub startup_guidance: Option<String>,
    /// Terminal size (render breakpoint input).
    pub width: u16,
    pub height: u16,
    /// Window message cap (config).
    pub window_cap: usize,
    page_guard: PageGuard,
    want_backfill: bool,
    running_sessions: HashSet<SessionId>,
    /// Workspaces collapsed via `h` (FR-001-03); `l` expands all.
    pub collapsed_workspaces: HashSet<crate::api::types::WorkspaceId>,
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
            collapsed_workspaces: HashSet::new(),
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
                // After recovery trigger refollow only once (no repeated
                // repair).
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
        // Void the in-flight page (generation is kept, guarding against stale
        // resurrection).
        self.page_guard.in_flight = false;
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
    }

    fn scroll_to_bottom(&mut self) {
        let len = self.active_window().map(|w| w.len()).unwrap_or(0);
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
}
