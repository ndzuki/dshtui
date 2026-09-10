//! `dshtui monitor` 独立状态机与事件循环（REQ-009 §5：`MonitorAppState`
//! mpsc 单写多读，与主界面 Chat AppState 共存于同一二进制）。
//!
//! - 事件经 mpsc 进单一 state（`02 §3` 拓扑，单写 reducer）；
//! - 模式机 Town ↔ Detail ↔ Chat ↔ Stats ↔ Filter；`q` 顶层退出进程；
//! - 2s `/agents` + 30s `/kb-stats` 轮询协程（指数退避，恢复自动续）；
//! - crossterm 鼠标点击：cell → 显示缩放 → 逻辑像素 → NPC 最近命中容差
//!   （幂等：同 NPC 不叠加 pane，AC-009-04）；
//! - chat 同 agent 多轮复用 sessionId（AC-009-06）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::api::envelope::Backoff;
use crate::api::monitor::{ChatRequest, ChatResponse, MonitorClient, WireAgent, WireKbStats};
use crate::api::types::SessionId;
use crate::config::Effective;
use crate::input::{Command, InputMode, KeyDecoder};
use crate::model::agent_town::TownScene;
use crate::model::kb_stats::{KbStatsError, KbStatsSnapshot};
use crate::model::{AgentRosterEntry, RosterSnapshot};
use crate::ui::agent_town::{PaintOutcome, TownCanvas};

/// 模式机（REQ-009 §5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorMode {
    #[default]
    Town,
    Detail,
    Chat,
    Stats,
    Filter,
}

/// 问答 pane 消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub who: ChatWho,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatWho {
    User,
    Agent,
    Error,
}

/// chat 会话状态（同 agent 多轮复用 sessionId，AC-009-06）。
#[derive(Debug, Clone, Default)]
pub struct MonitorChatState {
    /// 当前问答目标 agent。
    pub target: Option<SessionId>,
    /// 服务端返回的会话 id（多轮复用）。
    pub session_id: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub input: String,
    pub busy: bool,
}

impl MonitorChatState {
    /// 打开某 agent 的问答（同 agent 保留消息与 sessionId；换 agent 重置）。
    pub fn open(&mut self, sid: SessionId) {
        if self.target.as_ref() != Some(&sid) {
            self.target = Some(sid);
            self.session_id = None;
            self.messages.clear();
            self.input.clear();
            self.busy = false;
        }
    }

    pub fn reset(&mut self) {
        self.target = None;
        self.session_id = None;
        self.messages.clear();
        self.input.clear();
        self.busy = false;
    }
}

/// mpsc 事件（网络/计时/输入结果全部经此进单一 state）。
#[derive(Debug)]
pub enum MonitorEvent {
    Startup,
    PollAgents {
        entries: Vec<WireAgent>,
        finished: u32,
        /// 本轮轮询往返 ms（FR-009-07 poll 延迟口径）。
        poll_ms: f64,
    },
    PollAgentsError {
        error: String,
    },
    PollKb {
        stats: WireKbStats,
    },
    PollKbError {
        error: String,
    },
    ChatReply {
        sid: SessionId,
        resp: ChatResponse,
    },
    ChatFailed {
        sid: SessionId,
        error: String,
    },
    Tick {
        now_ms: u64,
    },
    Resize {
        width: u16,
        height: u16,
    },
}

/// 事件循环执行的命令。
#[derive(Debug)]
pub enum MonitorCmd {
    /// 发起 `/agent/chat`（执行于事件循环，结果经 mpsc 回投）。
    SendChat { sid: SessionId, body: ChatRequest },
    /// `q` 退出进程。
    Exit,
}

/// 鼠标点击命中：目标 NPC。
#[derive(Debug, Clone)]
pub struct NpcHit {
    pub sid: SessionId,
}

/// `dshtui monitor` 独立 AppState（单写 reducer）。
pub struct MonitorAppState {
    pub scene: TownScene,
    pub roster: RosterSnapshot,
    pub kb: Option<KbStatsSnapshot>,
    pub kb_error: Option<String>,
    pub mode: MonitorMode,
    pub focus: usize,
    /// roster 滚动偏移（FR-009-04 滚轮滚动；行数，渲染时 clamp 保焦点可见）。
    pub roster_scroll: usize,
    pub detail: Option<SessionId>,
    pub chat: MonitorChatState,
    pub filter: String,
    pub help_open: bool,
    pub exited: bool,
    pub toast: Option<String>,
    pub toast_until_ms: f64,
    pub connected: bool,
    pub last_error: Option<String>,
    /// 最近一次 /agents 轮询往返 ms（FR-009-07 poll 延迟口径）。
    pub poll_ms: f64,
    pub kitty_capable: bool,
    pub width: u16,
    pub height: u16,
    pub now_ms: f64,
    image_id: u32,
}

impl MonitorAppState {
    pub fn new(kitty_capable: bool, now_ms: f64, seed: u64) -> Self {
        Self {
            scene: TownScene::new(seed, now_ms),
            roster: RosterSnapshot::new(),
            kb: None,
            kb_error: None,
            mode: MonitorMode::Town,
            focus: 0,
            roster_scroll: 0,
            detail: None,
            chat: MonitorChatState::default(),
            filter: String::new(),
            help_open: false,
            exited: false,
            toast: None,
            toast_until_ms: 0.0,
            connected: false,
            last_error: None,
            poll_ms: 0.0,
            kitty_capable,
            width: 80,
            height: 24,
            now_ms,
            image_id: 1,
        }
    }

    /// 每轮 paint 使用的 kitty image id（进帧自增）。
    pub fn next_image_id(&mut self) -> u32 {
        let id = self.image_id;
        self.image_id = self.image_id.wrapping_add(1).max(1);
        id
    }

    /// 排序后的 roster 视图（stageKey 排序，HTML renderRoster 口径）。
    pub fn sorted_roster(&self) -> Vec<&AgentRosterEntry> {
        let mut items = self.roster.ordered();
        items.sort_by(|a, b| {
            a.stage_key()
                .cmp(b.stage_key())
                .then(a.session_id.cmp(&b.session_id))
        });
        items
    }

    /// 焦点 agent（roster 为空 → None）。
    pub fn focused_entry(&self) -> Option<&AgentRosterEntry> {
        let items = self.sorted_roster();
        items
            .get(self.focus.min(items.len().saturating_sub(1)))
            .copied()
    }

    fn clamp_focus(&mut self) {
        let len = self.sorted_roster().len();
        if len == 0 {
            self.focus = 0;
        } else {
            self.focus = self.focus.min(len - 1);
        }
    }

    /// 单写 reducer：事件 → 命令队列。
    pub fn handle(&mut self, ev: MonitorEvent) -> VecDeque<MonitorCmd> {
        match ev {
            MonitorEvent::Startup => {
                self.toast("连接 agent-server …".into());
                VecDeque::new()
            }
            MonitorEvent::PollAgents {
                entries,
                finished,
                poll_ms,
            } => {
                let (added, updated, removed) = self.roster.merge(&entries, finished);
                self.scene.sync_agents(
                    &self
                        .roster
                        .ordered()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                );
                self.connected = true;
                self.poll_ms = poll_ms;
                if self.last_error.is_some() {
                    self.toast(format!(
                        "agent-server 已恢复（+{added}/~{updated}/-{removed}）"
                    ));
                    self.last_error = None;
                }
                self.clamp_focus();
                VecDeque::new()
            }
            MonitorEvent::PollAgentsError { error } => {
                self.connected = false;
                self.last_error = Some(error);
                VecDeque::new()
            }
            MonitorEvent::PollKb { stats } => {
                match KbStatsSnapshot::from_wire(&stats) {
                    Ok(snap) => {
                        self.kb = Some(snap);
                        self.kb_error = None;
                    }
                    Err(KbStatsError::HistogramMismatch { boundaries, counts }) => {
                        // 契约漂移：保留旧快照 + 可读错误（REQ-009 §6）。
                        tracing::warn!(boundaries, counts, "/kb-stats 直方图契约漂移，保留旧快照");
                        self.kb_error = Some(format!(
                            "KB 直方图契约漂移（boundaries {boundaries} ≠ counts {counts}）"
                        ));
                    }
                }
                VecDeque::new()
            }
            MonitorEvent::PollKbError { error } => {
                self.kb_error = Some(error);
                VecDeque::new()
            }
            MonitorEvent::ChatReply { sid, resp } => {
                if let Some(st) = self.chat_for(sid.clone()) {
                    st.busy = false;
                    if !resp.session_id.is_empty() {
                        st.session_id = Some(resp.session_id);
                    }
                    if !resp.text.is_empty() {
                        st.messages.push(ChatMessage {
                            who: ChatWho::Agent,
                            text: resp.text,
                        });
                    } else if !resp.error.is_empty() || !resp.error_code.is_empty() {
                        st.messages.push(ChatMessage {
                            who: ChatWho::Error,
                            text: format!("⚠️ 业务错误 [{}]: {}", resp.error_code, resp.error),
                        });
                    } else {
                        st.messages.push(ChatMessage {
                            who: ChatWho::Agent,
                            text: "(无回复)".into(),
                        });
                    }
                }
                VecDeque::new()
            }
            MonitorEvent::ChatFailed { sid, error } => {
                if let Some(st) = self.chat_for(sid) {
                    st.busy = false;
                    st.messages.push(ChatMessage {
                        who: ChatWho::Error,
                        text: format!("发送失败: {error}（确认 dsh-agent-server 运行中）"),
                    });
                }
                VecDeque::new()
            }
            MonitorEvent::Tick { now_ms } => {
                self.now_ms = now_ms as f64;
                self.scene.advance(now_ms as f64);
                if self.toast_until_ms > 0.0 && self.toast_until_ms <= self.now_ms {
                    self.toast = None;
                    self.toast_until_ms = 0.0;
                }
                VecDeque::new()
            }
            MonitorEvent::Resize { width, height } => {
                self.width = width;
                self.height = height;
                VecDeque::new()
            }
        }
    }

    fn chat_for(&mut self, sid: SessionId) -> Option<&mut MonitorChatState> {
        if self.chat.target.as_ref() == Some(&sid) {
            Some(&mut self.chat)
        } else {
            None
        }
    }

    fn toast(&mut self, text: String) {
        self.toast = Some(text);
        self.toast_until_ms = self.now_ms + 2600.0;
    }

    /// 键位/输入命令 → 命令队列（vim 键与鼠标双轨等价，AC-009-05）。
    pub fn handle_command(&mut self, cmd: Command) -> VecDeque<MonitorCmd> {
        let mut out = VecDeque::new();
        match self.mode {
            MonitorMode::Chat | MonitorMode::Filter => match cmd {
                Command::ClosePicker => {
                    // Esc：Chat 保留会话（多轮），Filter 清除过滤。
                    if self.mode == MonitorMode::Filter {
                        self.filter.clear();
                    }
                    self.mode = MonitorMode::Town;
                }
                Command::PickerBackspace => {
                    if self.mode == MonitorMode::Chat {
                        self.chat.input.pop();
                    } else {
                        self.filter.pop();
                    }
                }
                Command::PickerInput(ch) => {
                    if self.mode == MonitorMode::Chat {
                        self.chat.input.push_str(&ch);
                    } else {
                        self.filter.push_str(&ch);
                    }
                }
                Command::SubmitInput => {
                    if self.mode == MonitorMode::Chat {
                        self.submit_chat(&mut out);
                    } else {
                        self.mode = MonitorMode::Town;
                    }
                }
                _ => {}
            },
            _ => match cmd {
                Command::MoveDown => {
                    self.focus = self.focus.saturating_add(1);
                    self.clamp_focus();
                }
                Command::MoveUp => {
                    self.focus = self.focus.saturating_sub(1);
                }
                Command::GotoTop => self.focus = 0,
                Command::GotoBottom => {
                    let len = self.sorted_roster().len();
                    self.focus = len.saturating_sub(1);
                }
                Command::OpenFocused => self.open_detail(None),
                Command::MonitorOpenChat => self.open_chat(None),
                Command::MonitorStats => {
                    if self.kb.is_some() {
                        self.mode = MonitorMode::Stats;
                    } else {
                        self.toast("KB 统计暂不可用（等待 /kb-stats）".into());
                    }
                }
                Command::MonitorCheer => {
                    if let Some(sid) = self.focused_entry().map(|e| e.session_id.clone()) {
                        if let Some(n) = self.scene.npc_mut(&sid) {
                            n.cheering = true;
                            n.cheer_until_ms = self.now_ms + 900.0; // HTML cheer 900ms 复位
                            self.toast(format!(
                                "💗 已给 {} 加油！",
                                crate::model::short_session(&sid.get())
                            ));
                        }
                    }
                }
                Command::MonitorLocate => {
                    // 先取展示字段（owned），再释放借用后变更状态。
                    let located = self.focused_entry().map(|e| {
                        (
                            e.display_name(),
                            crate::model::stage_meta(e.stage_key()).label,
                        )
                    });
                    if let Some((name, label)) = located {
                        self.detail = None;
                        self.mode = MonitorMode::Town;
                        self.toast(format!("📍 定位 {name}（{label}）"));
                    }
                }
                Command::StartSearch => {
                    self.filter.clear();
                    self.mode = MonitorMode::Filter;
                }
                Command::OpenHelp => self.help_open = true,
                Command::CloseHelp => self.help_open = false,
                Command::Quit => {
                    self.exited = true;
                    out.push_back(MonitorCmd::Exit);
                }
                Command::ClosePicker => {
                    // Town 下 Esc：关闭详情回 Town。
                    self.detail = None;
                    if self.mode == MonitorMode::Detail {
                        self.mode = MonitorMode::Town;
                    }
                }
                _ => {}
            },
        }
        out
    }

    /// Enter/点击打开详情（幂等：同 NPC 不叠加 pane，AC-009-04）。
    fn open_detail(&mut self, sid: Option<SessionId>) {
        let target = sid.or_else(|| self.focused_entry().map(|e| e.session_id.clone()));
        if let Some(sid) = target {
            if self.roster.get(&sid).is_some() {
                self.detail = Some(sid);
                self.mode = MonitorMode::Detail;
            }
        }
    }

    /// `c` 或详情 pane 点击 💬 打开问答（目标 = 指定/焦点 agent）。
    fn open_chat(&mut self, sid: Option<SessionId>) {
        let sid = sid.or_else(|| self.focused_entry().map(|e| e.session_id.clone()));
        if let Some(sid) = sid {
            self.chat.open(sid);
            self.mode = MonitorMode::Chat;
        }
    }

    /// 提交问答消息：首条带 kbQuery（任务标题）+ project；多轮复用
    /// sessionId（D-29/AC-009-06）。
    fn submit_chat(&mut self, out: &mut VecDeque<MonitorCmd>) {
        if self.chat.busy {
            return;
        }
        let text = self.chat.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(sid) = self.chat.target.clone() else {
            return;
        };
        let entry = self.roster.get(&sid).cloned();
        self.chat.input.clear();
        self.chat.busy = true;
        self.chat.messages.push(ChatMessage {
            who: ChatWho::User,
            text: text.clone(),
        });
        let fresh = self.chat.session_id.is_none();
        let provider = entry.as_ref().and_then(|e| {
            if e.provider.is_empty() {
                None
            } else {
                Some(e.provider.clone())
            }
        });
        let model = entry.as_ref().and_then(|e| {
            if e.model.is_empty() {
                None
            } else {
                Some(e.model.clone())
            }
        });
        let body = ChatRequest {
            message: text,
            provider: provider.unwrap_or_else(|| "deepseek_magic".into()),
            model: model.unwrap_or_else(|| "deepseek-v4-pro".into()),
            reasoning_effort: Some("medium".into()),
            session_id: self.chat.session_id.clone(),
            kb_query: if fresh {
                entry
                    .as_ref()
                    .map(|e| e.task_first_line())
                    .filter(|t| !t.is_empty())
            } else {
                None
            },
            project: entry.and_then(|e| {
                if e.project.is_empty() {
                    None
                } else {
                    Some(e.project)
                }
            }),
        };
        out.push_back(MonitorCmd::SendChat { sid, body });
    }

    /// 鼠标点击命中判定（AC-009-04）：逻辑像素 → NPC 最近（容差 24px，
    /// 与 HTML 命中框 26×32 同量级）；同 NPC 幂等。
    pub fn hit_npc(&self, logical: (u16, u16)) -> Option<NpcHit> {
        let (lx, ly) = logical;
        let mut best: Option<(f64, &crate::model::agent_town::TownNpc)> = None;
        let mut best_d = 24.0f64;
        for npc in self.scene.npcs.iter() {
            let d = ((npc.cx - lx as f64).powi(2) + (npc.cy - ly as f64).powi(2)).sqrt();
            if d < best_d {
                best_d = d;
                best = Some((d, npc));
            }
        }
        best.map(|(_, n)| NpcHit { sid: n.sid.clone() })
    }

    /// 滚轮滚动 roster（FR-009-04；clamp 到合法偏移）。
    pub fn scroll_roster(&mut self, delta: i32) {
        let len = self.sorted_roster().len();
        if len == 0 {
            self.roster_scroll = 0;
            return;
        }
        let mut s = self.roster_scroll as i64 + delta as i64;
        s = s.clamp(0, len.saturating_sub(1) as i64);
        self.roster_scroll = s as usize;
    }

    /// 过滤后的 roster（phase/status 子串匹配，FR-009-04 `/`）。
    pub fn filtered_roster(&self) -> Vec<&AgentRosterEntry> {
        let f = self.filter.trim().to_ascii_lowercase();
        if f.is_empty() {
            return self.sorted_roster();
        }
        self.sorted_roster()
            .into_iter()
            .filter(|e| {
                e.phase.to_ascii_lowercase().contains(&f)
                    || e.task_status.to_ascii_lowercase().contains(&f)
                    || e.status.as_str().contains(&f)
                    || e.stage_key().contains(&f)
                    || e.display_name().to_ascii_lowercase().contains(&f)
            })
            .collect()
    }
}

// ============================ 事件循环 ============================

/// 帧循环节拍（对齐主界面 tick 33ms）。
const TICK_MS: u64 = 33;

/// `dshtui monitor` 运行入口（main.rs 分派）。
pub async fn run(eff: Effective) -> Result<(), String> {
    let client = MonitorClient::new(&eff.monitor.addr).map_err(|e| e.to_string())?;
    let kitty = crate::ui::image::kitty_supported();
    let font = crate::ui::image::terminal_font_size();

    let mut terminal = MonitorTerminal::enter().map_err(|e| e.to_string())?;
    let (ev_tx, mut ev_rx) = mpsc::channel::<MonitorEvent>(256);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut app = MonitorAppState::new(kitty, now_ms as f64, now_ms ^ 0x9e37_79b9);
    let mut canvas = TownCanvas::new(app.next_image_id());
    let mut decoder = KeyDecoder::from_effective(&eff);

    // 启动立即发出首次 `/agents`（AC-009-01：1s 内进入界面并发出首请求）；
    // `/health` 启动探测并发（§6：失败即提示启动指引，不等待轮询）。
    spawn_poll_agents(client.clone(), eff.monitor.poll_agents_ms, ev_tx.clone());
    spawn_poll_kb(client.clone(), eff.monitor.poll_kb_ms, ev_tx.clone());
    spawn_health_probe(client.clone(), ev_tx.clone());
    let mut commands: VecDeque<MonitorCmd> = app.handle(MonitorEvent::Startup);
    let mut paint_at = Instant::now() - Duration::from_secs(1); // 首帧立即画
                                                                // perf 采样（Notes/06 §8 口径）：帧间隔 p50 + RSS；DSHTUI_PERF_LOG 开启
                                                                // 时每 ~5s 追加一行 `/tmp/dshtui-perf.log`。
    let mut sampler = crate::perf::FrameSampler::new();
    let perf_log = std::env::var("DSHTUI_PERF_LOG").unwrap_or_default();
    let mut last_perf_log = Instant::now();
    let mut last_malloc_trim = Instant::now();

    loop {
        // 关键：`#[tokio::main(flavor="current_thread")]` 下主循环若不
        // await，spawn 的 poll 协程会被饿死（2026-09-07 冒烟实测：首帧后
        // 状态永不更新）。每轮末尾 sleep 让出运行时驱动 poll 任务。
        while let Some(cmd) = commands.pop_front() {
            match cmd {
                MonitorCmd::Exit => app.exited = true,
                MonitorCmd::SendChat { sid, body } => {
                    let cl = client.clone();
                    let tx = ev_tx.clone();
                    tokio::spawn(async move {
                        let ev = match cl.chat(&body).await {
                            Ok(resp) => MonitorEvent::ChatReply { sid, resp },
                            Err(e) => MonitorEvent::ChatFailed {
                                sid,
                                error: e.to_string(),
                            },
                        };
                        let _ = tx.send(ev).await;
                    });
                }
            }
            if app.exited {
                break;
            }
        }
        if app.exited {
            break;
        }

        // paint（AC-009-03：有 agent 30fps；无 agent 静态降频 250ms）。
        if kitty && paint_at <= Instant::now() {
            let outcome = canvas.paint(&app.scene);
            paint_at = match outcome {
                PaintOutcome::Static => Instant::now() + Duration::from_millis(120),
                _ if !app.scene.has_agents() => Instant::now() + Duration::from_millis(250),
                _ => Instant::now() + Duration::from_millis(TICK_MS),
            };
        }

        terminal
            .terminal
            .draw(|frame| crate::ui::monitor::render(frame, &app))
            .map_err(|e| e.to_string())?;
        // 帧字节直接写 backend（显式 MoveTo 定位画面锚点），不驻留 ratatui
        // Buffer（RSS 口径 AC-009-09；write 失败降级为日志不崩溃）。
        if let Some(bytes) = canvas.take_emit() {
            use std::io::Write;
            let area = crate::ui::monitor::town_area(&app);
            let mut move_to = format!("\x1b[{};{}H", area.y + 1, area.x + 1);
            move_to.push_str(&bytes);
            let ok = terminal
                .terminal
                .backend_mut()
                .write_all(move_to.as_bytes())
                .is_ok();
            // 归还缓冲复用（capacity 保留，避免每帧大分配 churn）。
            canvas.put_back_emit(bytes);
            if !ok {
                tracing::warn!("kitty 帧写入失败，降级继续运行");
            }
        }
        sampler.tick();
        if !perf_log.is_empty()
            && sampler.count() > 0
            && last_perf_log.elapsed() >= Duration::from_secs(5)
        {
            crate::perf::log_perf_line(
                &perf_log,
                crate::perf::rss_mb(),
                sampler.p50(),
                app.poll_ms,
            );
            last_perf_log = Instant::now();
        }
        // glibc free-list 归还（kitty 帧编码的大缓冲 churn 会让 RSS 滞留，
        // 周期性 trim 保持 <20MB 口径，AC-009-09；仅 Linux/glibc，失败静默）。
        if last_malloc_trim.elapsed() >= Duration::from_secs(2) {
            crate::perf::malloc_trim();
            last_malloc_trim = Instant::now();
        }

        // 输入非阻塞检查（节拍由末尾 sleep 统一控制，避免双倍延迟拉低帧率）。
        if event::poll(Duration::ZERO).map_err(|e| e.to_string())? {
            let input = event::read().map_err(|e| e.to_string())?;
            match input {
                // 滚轮：滚动 roster（FR-009-04 双轨）。
                Event::Mouse(me)
                    if me.kind == MouseEventKind::ScrollDown
                        || me.kind == MouseEventKind::ScrollUp =>
                {
                    let delta = if me.kind == MouseEventKind::ScrollDown {
                        1
                    } else {
                        -1
                    };
                    app.scroll_roster(delta);
                }
                Event::Mouse(me) if me.kind == MouseEventKind::Down(MouseButton::Left) => {
                    // 详情 pane 底部 💬 行点击 → 对详情目标打开问答（AC-009-06
                    // 双入口：`c` 或点击 💬）。
                    if app.mode == MonitorMode::Detail {
                        let chat_row = crate::ui::monitor::detail_chat_row(&app);
                        if chat_row == me.row {
                            if let Some(sid) = app.detail.clone() {
                                app.open_chat(Some(sid));
                            }
                            continue;
                        }
                    }
                    // kitty 画面 NPC 命中 → 详情（幂等，AC-009-04）。
                    if kitty {
                        let area = crate::ui::monitor::town_area(&app);
                        if let Some(logical) =
                            TownCanvas::cell_to_logical(area, font, me.column, me.row)
                        {
                            if let Some(hit) = app.hit_npc(logical) {
                                // 幂等：同 NPC 重开详情不叠加 pane（AC-009-04）。
                                app.open_detail(Some(hit.sid));
                            }
                        }
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Chat/Filter 输入态复用 Insert 语义（字符/退格/Enter/Esc）。
                    let mode = if app.help_open {
                        InputMode::Help
                    } else {
                        match app.mode {
                            MonitorMode::Chat | MonitorMode::Filter => InputMode::Insert,
                            _ => InputMode::Monitor,
                        }
                    };
                    if let Some(cmd) = decoder.decode(mode, Event::Key(key)) {
                        commands.extend(app.handle_command(cmd));
                    }
                }
                Event::Resize(w, h) => {
                    commands.extend(app.handle(MonitorEvent::Resize {
                        width: w,
                        height: h,
                    }));
                    canvas.force_full_redraw();
                }
                _ => {}
            }
        }
        while let Ok(ev) = ev_rx.try_recv() {
            commands.extend(app.handle(ev));
        }
        commands.extend(
            app.handle(MonitorEvent::Tick {
                now_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            }),
        );
        // 让出 current_thread 运行时：poll/chat 协程推进 + 帧节拍。
        tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
    }
    Ok(())
}

/// 启动健康探测（§6）：失败立即提示启动指引（不等待轮询），成功后
/// 由 PollAgents 置 connected（探测仅加速错误面，不覆盖轮询状态）。
fn spawn_health_probe(client: MonitorClient, tx: mpsc::Sender<MonitorEvent>) {
    tokio::spawn(async move {
        if let Err(e) = client.health().await {
            tracing::warn!(error = %e, "/health 启动探测失败");
            let _ = tx
                .send(MonitorEvent::PollAgentsError {
                    error: format!("agent-server 不可达: {e}"),
                })
                .await;
        }
    });
}

/// `/agents` 轮询协程：2s 间隔；失败指数退避（上限 10s），恢复自动续
/// （REQ-009 §6：错误不刷屏，轮询不中断）。
fn spawn_poll_agents(client: MonitorClient, poll_ms: u64, tx: mpsc::Sender<MonitorEvent>) {
    tokio::spawn(async move {
        let mut backoff = Backoff::new(500, 10_000);
        loop {
            // 实测轮询往返（FR-009-07 poll 延迟口径）。
            let started = std::time::Instant::now();
            let result = client.agents().await;
            let took_ms = started.elapsed().as_secs_f64() * 1000.0;
            let delay = match result {
                Ok(resp) => {
                    backoff.reset();
                    tracing::debug!(
                        n = resp.entries.len(),
                        finished = resp.finished,
                        poll_ms = took_ms,
                        "/agents 轮询成功"
                    );
                    if tx
                        .send(MonitorEvent::PollAgents {
                            entries: resp.entries,
                            finished: resp.finished,
                            poll_ms: took_ms,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    poll_ms
                }
                Err(e) => {
                    let extra = backoff.next_delay_ms();
                    tracing::warn!(error = %e, delay_ms = extra, "/agents 轮询失败，退避重试");
                    if tx
                        .send(MonitorEvent::PollAgentsError {
                            error: e.to_string(),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    poll_ms.saturating_add(extra)
                }
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    });
}

/// `/kb-stats` 轮询协程：30s 间隔；失败退避（不阻塞 agents 轮询）。
fn spawn_poll_kb(client: MonitorClient, poll_ms: u64, tx: mpsc::Sender<MonitorEvent>) {
    tokio::spawn(async move {
        let mut backoff = Backoff::new(500, 10_000);
        loop {
            let delay = match client.kb_stats().await {
                Ok(stats) => {
                    backoff.reset();
                    tracing::debug!(hits = stats.totals.hits, "/kb-stats 轮询成功");
                    if tx.send(MonitorEvent::PollKb { stats }).await.is_err() {
                        return;
                    }
                    poll_ms
                }
                Err(e) => {
                    let extra = backoff.next_delay_ms();
                    tracing::warn!(error = %e, delay_ms = extra, "/kb-stats 轮询失败，退避重试");
                    if tx
                        .send(MonitorEvent::PollKbError {
                            error: e.to_string(),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    poll_ms.saturating_add(extra)
                }
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    });
}

/// 终端会话（enter/退出恢复；与 main.rs TerminalSession 同语义）。
struct MonitorTerminal {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl MonitorTerminal {
    fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
        crossterm::execute!(stdout, crossterm::event::EnableMouseCapture)?;
        crossterm::execute!(stdout, crossterm::cursor::Hide)?;
        let backend = CrosstermBackend::new(stdout);
        Ok(Self {
            terminal: Terminal::new(backend)?,
        })
    }
}

impl Drop for MonitorTerminal {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::event::DisableMouseCapture
        );
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

// ============================ 状态机测试 ============================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::monitor::WireAgent;
    use crate::input::Command;

    fn wire(sid: &str, task_status: &str, status: &str) -> WireAgent {
        WireAgent {
            session_id: sid.into(),
            phase: "".into(),
            task: "任务A".into(),
            project: "proj".into(),
            task_id: "TASK-1".into(),
            status: status.into(),
            task_status: task_status.into(),
            elapsed: 100,
            last_event_at: 0,
            seq: 1,
            label: "".into(),
            kind: "session".into(),
            parent_session_id: "".into(),
            delegation_depth: 0,
            provider: "".into(),
            model: "".into(),
        }
    }

    fn app_with_two() -> MonitorAppState {
        let mut app = MonitorAppState::new(false, 0.0, 1);
        let entries = vec![
            wire("session-a", "implementing", "working"),
            wire("session-b", "", "idle"),
        ];
        let finished = 3;
        app.handle(MonitorEvent::PollAgents {
            entries,
            finished,
            poll_ms: 2.0,
        });
        app
    }

    #[test]
    fn poll_agents_updates_roster_and_scene_and_status() {
        let mut app = MonitorAppState::new(false, 0.0, 1);
        assert!(!app.connected);
        let entries = vec![wire("session-a", "implementing", "working")];
        app.handle(MonitorEvent::PollAgents {
            entries: entries.clone(),
            finished: 5,
            poll_ms: 2.0,
        });
        assert!(app.connected);
        assert_eq!(app.roster.len(), 1);
        assert_eq!(
            app.roster.finished, 5,
            "状态条完工数读 x-agents-finished（ADR-008）"
        );
        assert_eq!(app.scene.npcs.len(), 1, "NPC 数量与 roster 同步");
        // 同数据重复 poll 幂等（AC-009-10）。
        app.handle(MonitorEvent::PollAgents {
            entries,
            finished: 5,
            poll_ms: 2.0,
        });
        assert_eq!(app.roster.len(), 1);
        assert_eq!(app.scene.npcs.len(), 1);
    }

    #[test]
    fn poll_error_marks_disconnected_and_keeps_snapshot() {
        let mut app = app_with_two();
        app.handle(MonitorEvent::PollAgentsError {
            error: "connection refused".into(),
        });
        assert!(!app.connected);
        assert!(app.last_error.is_some());
        assert_eq!(app.roster.len(), 2, "失败保留上一快照不清空（REQ-009 §6）");
        assert_eq!(app.scene.npcs.len(), 2);
        // 恢复路径：下一次成功轮询清除错误并续上。
        app.handle(MonitorEvent::PollAgents {
            entries: vec![wire("session-a", "implementing", "working")],
            finished: 0,
            poll_ms: 2.0,
        });
        assert!(app.connected);
        assert_eq!(app.roster.len(), 1);
    }

    #[test]
    fn vim_navigation_and_detail_are_idempotent() {
        let mut app = app_with_two();
        // roster 按 stageKey 排序：idle（session-b）在前。
        assert_eq!(app.focused_entry().unwrap().session_id.get(), "session-b");
        app.handle_command(Command::MoveDown);
        assert_eq!(app.focused_entry().unwrap().session_id.get(), "session-a");
        app.handle_command(Command::MoveDown); // 越界 clamp
        assert_eq!(app.focused_entry().unwrap().session_id.get(), "session-a");
        app.handle_command(Command::GotoTop);
        assert_eq!(app.focus, 0);
        assert_eq!(app.focused_entry().unwrap().session_id.get(), "session-b");
        app.handle_command(Command::GotoBottom);
        assert_eq!(app.focus, 1);

        // Enter 开详情；重复 Enter 幂等（不叠加 pane，AC-009-04）。
        app.handle_command(Command::OpenFocused);
        assert_eq!(app.mode, MonitorMode::Detail);
        assert_eq!(app.detail.as_ref().unwrap().get(), "session-a");
        app.handle_command(Command::OpenFocused);
        assert_eq!(app.detail.as_ref().unwrap().get(), "session-a");

        // Esc 关闭详情回 Town。
        app.handle_command(Command::ClosePicker);
        assert_eq!(app.mode, MonitorMode::Town);
        assert!(app.detail.is_none());
    }

    #[test]
    fn mouse_hit_opens_detail_and_is_idempotent() {
        let mut app = app_with_two();
        // 让 agent 出现在已知位置：sync 后 NPC 出生在广场 (480,300)。
        let npc = &app.scene.npcs[0];
        let (cx, cy) = (npc.cx, npc.cy);
        let hit = app
            .hit_npc((cx.round() as u16, cy.round() as u16))
            .expect("广场出生点应命中");
        assert_eq!(hit.sid.get(), "session-a");
        app.open_detail(Some(hit.sid.clone()));
        assert_eq!(app.detail.as_ref().unwrap().get(), "session-a");
        // 同 NPC 重复点击幂等。
        app.open_detail(Some(hit.sid));
        assert_eq!(app.detail.as_ref().unwrap().get(), "session-a");
        // 远离 NPC 的点击不命中（容差 24px）。
        assert!(app.hit_npc((20, 20)).is_none());
    }

    #[test]
    fn chat_mode_multi_turn_reuses_session_id() {
        let mut app = app_with_two();
        app.handle_command(Command::MonitorOpenChat);
        assert_eq!(app.mode, MonitorMode::Chat);
        // 焦点 = stageKey 排序首位（idle → session-b）。
        let sid = app.chat.target.clone().unwrap();
        assert_eq!(sid.get(), "session-b");
        assert!(app.chat.session_id.is_none());

        // 输入消息 + 提交 → busy + SendChat（首条带 kbQuery/project）。
        app.handle_command(Command::PickerInput("你好".into()));
        let mut cmds = app.handle_command(Command::SubmitInput);
        assert!(app.chat.busy);
        let cmd = cmds.pop_front().expect("应发出 SendChat");
        let body = match cmd {
            MonitorCmd::SendChat { body, .. } => body,
            other => panic!("期望 SendChat，实际 {other:?}"),
        };
        assert_eq!(body.message, "你好");
        assert_eq!(body.session_id, None, "首条无 sessionId");
        assert_eq!(
            body.kb_query.as_deref(),
            Some("任务A"),
            "首条带任务标题 kbQuery"
        );
        assert_eq!(body.project.as_deref(), Some("proj"));

        // 服务端回复 sessionId → 多轮复用。
        app.handle(MonitorEvent::ChatReply {
            sid: sid.clone(),
            resp: ChatResponse {
                text: "回答1".into(),
                outcome: "completed".into(),
                session_id: "session-chat-1".into(),
                ..Default::default()
            },
        });
        assert!(!app.chat.busy);
        assert_eq!(app.chat.session_id.as_deref(), Some("session-chat-1"));
        assert_eq!(app.chat.messages.len(), 2);

        app.handle_command(Command::PickerInput("再问".into()));
        let mut cmds2 = app.handle_command(Command::SubmitInput);
        let body2 = match cmds2.pop_front().unwrap() {
            MonitorCmd::SendChat { body, .. } => body,
            other => panic!("期望 SendChat，实际 {other:?}"),
        };
        assert_eq!(
            body2.session_id.as_deref(),
            Some("session-chat-1"),
            "多轮复用 sessionId（AC-009-06）"
        );
        assert_eq!(body2.kb_query, None, "多轮不再带 kbQuery");
    }

    #[test]
    fn chat_failure_is_readable_and_recoverable() {
        let mut app = app_with_two();
        app.handle_command(Command::MonitorOpenChat);
        let sid = app.chat.target.clone().unwrap();
        app.handle_command(Command::PickerInput("问".into()));
        let _ = app.handle_command(Command::SubmitInput);
        app.handle(MonitorEvent::ChatFailed {
            sid,
            error: "网络传输失败: /agent/chat 请求失败（timeout）".into(),
        });
        assert!(!app.chat.busy);
        let last = app.chat.messages.last().unwrap();
        assert_eq!(last.who, ChatWho::Error);
        assert!(
            last.text.contains("发送失败"),
            "失败提示可读: {}",
            last.text
        );
        // 恢复路径：再次提交可正常走发送（状态未被旧失败污染）。
        app.handle_command(Command::PickerInput("重试".into()));
        let cmds = app.handle_command(Command::SubmitInput);
        assert_eq!(cmds.len(), 1);
        assert!(app.chat.busy);
    }

    #[test]
    fn kb_stats_event_and_histogram_drift() {
        let mut app = app_with_two();
        app.handle(MonitorEvent::PollKb {
            stats: valid_kb_stats(8),
        });
        assert!(app.kb.is_some());
        assert_eq!(app.kb.as_ref().unwrap().totals.hits, 8);

        // 契约漂移：边界/counts 不等长 → 保留旧快照 + 可读错误。
        let mut bad = valid_kb_stats(9);
        bad.totals.hist.counts = vec![1; 3];
        app.handle(MonitorEvent::PollKb { stats: bad });
        assert!(app.kb_error.is_some());
        assert_eq!(app.kb.as_ref().unwrap().totals.hits, 8, "漂移保留旧快照");
    }

    #[test]
    fn stats_key_opens_only_when_available() {
        let mut app = app_with_two();
        app.handle_command(Command::MonitorStats);
        assert_ne!(app.mode, MonitorMode::Stats, "无数据时不开 Stats");
        assert!(app.toast.is_some());
        app.handle(MonitorEvent::PollKb {
            stats: valid_kb_stats(0),
        });
        app.handle_command(Command::MonitorStats);
        assert_eq!(app.mode, MonitorMode::Stats);
    }

    #[test]
    fn filter_mode_matches_phase_and_status() {
        let mut app = app_with_two();
        app.handle_command(Command::StartSearch);
        assert_eq!(app.mode, MonitorMode::Filter);
        app.handle_command(Command::PickerInput("implementing".into()));
        assert_eq!(app.filtered_roster().len(), 1);
        assert_eq!(app.filtered_roster()[0].session_id.get(), "session-a");
        // status 过滤（idle）——重新开过滤输入。
        app.handle_command(Command::ClosePicker);
        app.handle_command(Command::StartSearch);
        app.handle_command(Command::PickerInput("idle".into()));
        let hits = app.filtered_roster();
        assert_eq!(hits.len(), 1, "idle 过滤只命中 session-b");
        assert!(hits.iter().any(|e| e.session_id.get() == "session-b"));
        // Esc 退出过滤。
        app.handle_command(Command::ClosePicker);
        assert_eq!(app.mode, MonitorMode::Town);
        assert!(app.filter.is_empty());
    }

    fn valid_kb_stats(hits: i64) -> crate::api::monitor::WireKbStats {
        let bucket = |hits: i64| crate::api::monitor::WireKbBucket {
            hits,
            misses: 2,
            empty: 0,
            errs: 0,
            skipped: 0,
            searches: hits + 2,
            avg_ms: 100,
            hist: crate::api::monitor::WireHistogram {
                boundaries: crate::model::KB_DURATION_BOUNDARIES.to_vec(),
                counts: vec![1; 7],
            },
        };
        crate::api::monitor::WireKbStats {
            totals: bucket(hits),
            window: bucket(0),
            last_log_at: 0,
            restored: false,
        }
    }

    #[test]
    fn quit_exits_process() {
        let mut app = app_with_two();
        let cmds = app.handle_command(Command::Quit);
        assert!(app.exited);
        assert_eq!(cmds.len(), 1);
    }

    #[test]
    fn cheer_and_locate_toast_without_crash() {
        let mut app = app_with_two();
        app.handle_command(Command::MonitorCheer);
        assert!(app.toast.is_some());
        // 焦点 = stageKey 排序首位（session-b → idle）；加油动效触发在焦点 NPC。
        let focused_sid = app.focused_entry().unwrap().session_id.clone();
        let cheered = app
            .scene
            .npcs
            .iter()
            .find(|n| n.sid == focused_sid)
            .map(|n| n.cheering)
            .unwrap_or(false);
        assert!(cheered, "焦点 NPC 加油动效触发");
        app.handle_command(Command::MonitorLocate);
        assert!(app.toast.is_some());
        assert!(app.detail.is_none());
        assert_eq!(app.mode, MonitorMode::Town);
    }

    #[test]
    fn tick_advances_scene_and_expires_toast() {
        let mut app = app_with_two();
        app.handle_command(Command::MonitorCheer);
        assert!(app.toast.is_some());
        app.handle(MonitorEvent::Tick { now_ms: 5000 });
        assert!(app.toast.is_none(), "toast 过期清除");
        assert!(app.scene.now_ms > 0.0);
    }
}

#[cfg(test)]
mod review_fix_tests {
    use super::*;
    use crate::api::monitor::WireAgent;
    use crate::input::Command;

    fn wire(sid: &str, task_status: &str, status: &str) -> WireAgent {
        WireAgent {
            session_id: sid.into(),
            phase: "".into(),
            task: "任务A".into(),
            project: "proj".into(),
            task_id: "TASK-1".into(),
            status: status.into(),
            task_status: task_status.into(),
            elapsed: 100,
            last_event_at: 0,
            seq: 1,
            label: "".into(),
            kind: "session".into(),
            parent_session_id: "".into(),
            delegation_depth: 0,
            provider: "".into(),
            model: "".into(),
        }
    }

    fn app_with_n(n: usize) -> MonitorAppState {
        let mut app = MonitorAppState::new(false, 0.0, 3);
        let entries: Vec<WireAgent> = (0..n)
            .map(|i| {
                wire(
                    &format!("session-{i}"),
                    "",
                    if i % 2 == 0 { "working" } else { "idle" },
                )
            })
            .collect();
        app.handle(MonitorEvent::PollAgents {
            entries,
            finished: 0,
            poll_ms: 1.5,
        });
        app
    }

    #[test]
    fn poll_ms_round_trip_is_recorded() {
        let app = app_with_n(1);
        assert_eq!(app.poll_ms, 1.5, "FR-009-07 poll 延迟口径");
    }

    #[test]
    fn wheel_scroll_roster_clamps_to_bounds() {
        let mut app = app_with_n(5);
        app.scroll_roster(3);
        assert_eq!(app.roster_scroll, 3);
        app.scroll_roster(50);
        assert_eq!(app.roster_scroll, 4, "下界 clamp len-1");
        app.scroll_roster(-50);
        assert_eq!(app.roster_scroll, 0, "上界 clamp 0");
        // 空 roster：滚轮归零不 panic。
        let mut empty = MonitorAppState::new(false, 0.0, 1);
        empty.scroll_roster(5);
        assert_eq!(empty.roster_scroll, 0);
    }

    #[test]
    fn cheer_resets_after_900ms() {
        let mut app = app_with_n(1);
        app.handle_command(Command::MonitorCheer);
        let npc = &app.scene.npcs[0];
        assert!(npc.cheering, "加油立即生效");
        // 推进 1s → 复位（HTML cheer 900ms 口径）。
        app.handle(MonitorEvent::Tick { now_ms: 1000 });
        let npc = &app.scene.npcs[0];
        assert!(!npc.cheering, "900ms 后 cheer 复位，不再永久卡姿势");
    }

    #[test]
    fn open_chat_with_explicit_sid_supports_click_entry() {
        // AC-009-06 双入口：`c`（焦点）与点击 💬（指定 sid）走同一 open_chat。
        let mut app = app_with_n(2);
        app.open_chat(Some(crate::api::types::SessionId::new("session-1".into())));
        assert_eq!(app.mode, MonitorMode::Chat);
        assert_eq!(app.chat.target.as_ref().unwrap().get(), "session-1");
        // 换目标：会话重置（新会话）。
        app.open_chat(Some(crate::api::types::SessionId::new("session-0".into())));
        assert_eq!(app.chat.target.as_ref().unwrap().get(), "session-0");
        assert!(app.chat.messages.is_empty());
    }
}
