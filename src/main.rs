//! dshtui entry point: configuration, remote connection, and the terminal
//! event loop. The client never starts `dsh web`; connection failures stay
//! visible and actionable (AC-001-02).

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use dshtui::api::session;
use dshtui::api::types::{SessionAddress, SessionId, SessionSeq};
use dshtui::api::workspace;
use dshtui::api::{Backoff, ClientError, DshClient, Mux};
use dshtui::app::{AppEvent, AppState, Cmd, Mode};
use dshtui::config::{self, Cli, CliAction, Effective};
use dshtui::input::{Command, InputMode, KeyDecoder};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024; // REQ §6: 5MB log rotation.

/// `session/search` 截止时间（REQ-003 §6：unary 不得挂死帧循环）。
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
/// 搜索防抖窗口（AC-003-14：连续输入只保留最新）。
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// `loadThrough` 单页条数（Notes/03 §4.4：200 条/页覆盖目标 seq）。
const LOAD_THROUGH_PAGE_SIZE: usize = 200;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // parse_cli 兼容完整 argv 形式（自跳 argv[0]）——这里不预 skip，
    // 否则 `dshtui monitor` 位置参数会被当作程序名二次跳过。
    let args: Vec<String> = std::env::args().collect();
    let cli: Cli = match config::parse_cli(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误: {e}\n\n{}", config::usage_text());
            return ExitCode::from(2);
        }
    };
    match cli.action {
        CliAction::Help => {
            print!("{}", config::usage_text());
            return ExitCode::SUCCESS;
        }
        CliAction::Version => {
            println!("dshtui {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        CliAction::Run => {}
        CliAction::Monitor { .. } => {}
    }

    let log_path = cli
        .log_file
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(default_log_path);
    // REQ §3/§6: all errors go to the rotating file log (never the terminal).
    let _guard = match init_logging(&log_path) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("警告: 日志初始化失败（{error}），继续运行但无文件日志");
            None
        }
    };

    let cfg = match config::Config::load(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误: {e}");
            return ExitCode::from(2);
        }
    };
    let eff = match cfg.resolve(&cli) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("错误: {e}");
            return ExitCode::from(2);
        }
    };

    if !config::is_loopback(&eff.url) {
        eprintln!(
            "警告: 目标地址 {} 不是 loopback，认证 cookie 将发送到非本机地址，请确认这是你信任的服务器。",
            eff.url
        );
    }

    // REQ-009：`dshtui monitor` 子命令分派（独立事件循环，REQ-009 §5）。
    if matches!(cli.action, CliAction::Monitor { .. }) {
        if !config::is_loopback(&eff.monitor.addr) {
            eprintln!(
                "警告: agent-server 地址 {} 不是 loopback（REQ-009 §7 默认仅连本机回环）。",
                eff.monitor.addr
            );
        }
        return match dshtui::app::monitor::run(eff).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(1)
            }
        };
    }

    // FR-001-01: token may also be entered interactively (no echo, no history).
    match run_remote(eff.clone(), eff.token.clone()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::from(1)
        }
    }
}

fn default_log_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".local/state/dshtui/dshtui.log")
}

/// Initialize tracing to a rotating 5MB file log (REQ §6). Returns the guard
/// that keeps the non-blocking writer alive for the process lifetime.
fn init_logging(
    path: &Path,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>, String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("创建日志目录 {} 失败: {e}", dir.display()))?;
    }
    let writer = RotatingFile::new(path).map_err(|e| e.to_string())?;
    let (non_blocking, guard) = tracing_appender::non_blocking(writer);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(tracing_subscriber::filter::LevelFilter::INFO.into()),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .try_init()
        .map_err(|e| format!("tracing 初始化失败: {e}"))?;
    Ok(Some(guard))
}

/// io::Write wrapper that rotates the file when it exceeds LOG_ROTATE_BYTES:
/// the oversized file is renamed to `<name>.old` (previous `.old` replaced)
/// and a fresh file is opened.
struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
}

impl RotatingFile {
    fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let old = self.path.with_extension("log.old");
        let _ = fs::remove_file(&old);
        if self.path.exists() {
            fs::rename(&self.path, &old)?;
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(buf.len() as u64) > LOG_ROTATE_BYTES {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Startup probe (ADR-001: never spawn the backend). A failed probe — or a
/// missing token — enters the guidance screen: `r` re-probes, `i` pastes the
/// token (raw mode: no echo, no history), `q`/`Ctrl+c` exits.
async fn run_remote(eff: Effective, token: Option<String>) -> Result<(), String> {
    match token {
        Some(token) => match DshClient::connect(&eff.url, &token).await {
            Ok(client) => run_connected(eff, token, client).await,
            Err(error) => run_startup_guidance(eff, Some(token), error).await,
        },
        None => {
            run_startup_guidance(eff, None, ClientError::Auth("未提供 token".to_string())).await
        }
    }
}

async fn run_startup_guidance(
    eff: Effective,
    mut token: Option<String>,
    error: ClientError,
) -> Result<(), String> {
    let mut app = AppState::new(eff.perf.window_messages);
    app.handle(AppEvent::StartupProbeFailed(error.to_string()));
    let mut terminal = TerminalSession::enter().map_err(|e| e.to_string())?;
    let mut decoder = KeyDecoder::from_effective(&eff);
    let mut entering = false;
    let mut draft = String::new();

    fn sync_guidance(app: &mut AppState, entering: bool, has_token: bool, draft_len: usize) {
        let hint = if entering {
            format!(
                "正在输入 token（不显示原文）: {} 字符 — Enter 连接，Esc 取消",
                "*".repeat(draft_len)
            )
        } else if has_token {
            "[r] 重试连接  [i] 重新输入 token  [q] 退出".to_string()
        } else {
            "请设置 DSH_TOKEN 或按 [i] 粘贴 token（不落盘、不回显）— [q] 退出".to_string()
        };
        app.handle(AppEvent::StartupProbeFailed(hint));
    }
    sync_guidance(&mut app, false, token.is_some(), 0);

    loop {
        terminal
            .terminal
            .draw(|frame| dshtui::ui::render(frame, &app))
            .map_err(|e| e.to_string())?;
        if app.exited {
            return Ok(());
        }
        if !event::poll(Duration::from_millis(eff.ui.tick_ms)).map_err(|e| e.to_string())? {
            continue;
        }
        let input = event::read().map_err(|e| e.to_string())?;
        let mode = if entering {
            InputMode::Insert
        } else {
            InputMode::Normal
        };
        let Some(command) = decoder.decode(mode, input) else {
            continue;
        };

        match (entering, command) {
            (true, Command::PickerInput(ch)) => {
                draft.push_str(&ch);
                sync_guidance(&mut app, true, token.is_some(), draft.len());
            }
            (true, Command::PickerBackspace) => {
                draft.pop();
                sync_guidance(&mut app, true, token.is_some(), draft.len());
            }
            (true, Command::SubmitInput) => {
                let entered = draft.trim().to_string();
                draft.clear();
                entering = false;
                if entered.is_empty() {
                    sync_guidance(&mut app, false, token.is_some(), 0);
                    continue;
                }
                token = Some(entered.clone());
                match DshClient::connect(&eff.url, &entered).await {
                    Ok(client) => {
                        return run_connected(eff, entered, client).await;
                    }
                    Err(error) => {
                        app.handle(AppEvent::StartupProbeFailed(error.to_string()));
                        sync_guidance(&mut app, false, token.is_some(), 0);
                    }
                }
            }
            (true, Command::ClosePicker) => {
                entering = false;
                draft.clear();
                sync_guidance(&mut app, false, token.is_some(), 0);
            }
            (false, Command::InsertMode) => {
                entering = true;
                draft.clear();
                sync_guidance(&mut app, true, token.is_some(), 0);
            }
            (false, Command::RetryProbe) => {
                if let Some(current) = token.clone() {
                    match DshClient::connect(&eff.url, &current).await {
                        Ok(client) => return run_connected(eff, current, client).await,
                        Err(error) => {
                            app.handle(AppEvent::StartupProbeFailed(error.to_string()));
                            sync_guidance(&mut app, false, token.is_some(), 0);
                        }
                    }
                }
            }
            (false, Command::Quit) => {
                app.exited = true;
            }
            _ => {}
        }
    }
}

/// Main loop: single AppState, command queue, mpsc async events, one terminal.
/// One command executes per iteration so every network await is followed by a
/// frame — the first sidebar screen renders after the first `session/list`
/// page instead of after the full pagination (AC-001-03).
async fn run_connected(eff: Effective, token: String, client: DshClient) -> Result<(), String> {
    let config_path = dshtui::config::default_config_path();
    let mut terminal = TerminalSession::enter().map_err(|e| e.to_string())?;
    let mut app = AppState::new(eff.perf.window_messages);
    // REQ-004：启动检测一次 Kitty 能力（06 §6）+ 注入图片缓存预算。
    app.kitty_capable = dshtui::ui::image::kitty_supported();
    app.set_cache_budget(eff.perf.cache_bytes);
    // REQ-005：详情列宽从 `[ui].details_width_cells` 注入（默认 45）。
    app.details_width_cells = eff.ui.details_width_cells;
    // REQ-007：主题/palette 从 `[ui] theme/palette` 注入（AC-007-20）；非法
    // 覆盖只警告不崩溃。
    let palette_warnings = app.apply_palette_config(&eff.ui.theme, &eff.ui.palette);
    for w in &palette_warnings {
        eprintln!("警告: {w}");
    }
    // REQ-007：草稿持久化接线（AC-007-22/ADR-010）——drafts.toml 路径注入 +
    // 启动恢复/clear。
    app.drafts_enabled = eff.drafts.enabled;
    if app.drafts_enabled {
        app.drafts_path = Some(dshtui::config::default_state_path());
        if eff.drafts.clear {
            // 启动清空：内存 + 落盘均清（空表立即写回）。
            app.clear_all_drafts();
            flush_drafts(&mut app);
        } else if app.drafts_path.as_ref().is_some_and(|p| p.exists()) {
            let path = app.drafts_path.clone().unwrap();
            match std::fs::read_to_string(&path) {
                Ok(raw) => match dshtui::model::DraftStore::from_toml(&raw) {
                    Ok(store) => app.seed_drafts_from_store(store),
                    Err(e) => eprintln!("警告: drafts.toml 解析失败，回退内存注册表: {e}"),
                },
                Err(e) => eprintln!("警告: drafts.toml 读取失败: {e}"),
            }
        }
    }
    let mut decoder = KeyDecoder::from_effective(&eff);
    // REQ-007 AC-007-21：把生效覆盖行注入 AppState（帮助面板「我的键位」）。
    if !eff.keymap.modes.is_empty() {
        let km = dshtui::input::Keymap::build(&eff.keymap);
        app.keymap_override_lines = km.override_lines();
    }
    let mut client = Some(client);
    let mut mux: Option<Mux> = None;
    // Stream generations: stale stream tasks from a replaced mux must not
    // trigger reconnect storms after the connection is already healthy.
    let mux_generation = Arc::new(AtomicU64::new(0));
    let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(256);
    let mut commands = VecDeque::from(app.handle(AppEvent::Startup));
    let mut backoff = Backoff::new(500, 10_000);

    loop {
        // Reconnect: draw first, then back off, then re-probe (Notes/03 §8).
        if matches!(commands.front(), Some(Cmd::Reconnect { .. })) {
            commands.pop_front();
            terminal
                .terminal
                .draw(|frame| dshtui::ui::render(frame, &app))
                .map_err(|e| e.to_string())?;
            let delay = backoff.next_delay_ms();
            tokio::time::sleep(Duration::from_millis(delay)).await;
            match DshClient::connect(&eff.url, &token).await {
                Ok(new_client) => {
                    client = Some(new_client);
                    // Drop the dead mux; bump the generation so streams from it
                    // silently die instead of re-triggering disconnects.
                    mux = None;
                    mux_generation.fetch_add(1, Ordering::Relaxed);
                    backoff.reset();
                    let mut cmds = app.handle(AppEvent::Reconnected);
                    // Rebuild the workspace subscription (Step 2 contract).
                    cmds.push(Cmd::OpenWorkspaceFollow);
                    commands.extend(cmds);
                }
                Err(error) => {
                    tracing::warn!(error = %error, delay_ms = delay, "reconnect attempt failed");
                    let cmds = app.handle(AppEvent::Disconnected(format!("重连失败: {error}")));
                    commands.extend(cmds);
                }
            }
            continue;
        }

        // One command per iteration: list pagination renders between pages.
        if let Some(command) = commands.pop_front() {
            // REQ-007 `:edit`（AC-007-25）：需要 TerminalSession 释放/恢复
            // raw mode，主循环内联处理（execute_one 无 terminal 访问）。
            if let Cmd::ExternalEdit { tmp_path, editor } = &command {
                let mut outcome = Err(String::from("编辑器未运行"));
                match terminal.suspend_for_editor() {
                    Ok(()) => {
                        outcome = run_editor_blocking(tmp_path, editor);
                        if let Err(e) = terminal.resume_from_editor() {
                            app.last_error = Some(format!("终端恢复失败: {e}"));
                        }
                    }
                    Err(e) => {
                        app.last_error = Some(format!("终端挂起失败: {e}"));
                    }
                }
                let (ok, text, message) = match outcome {
                    Ok(()) => {
                        let text = std::fs::read_to_string(tmp_path).unwrap_or_default();
                        (true, text, String::new())
                    }
                    Err(msg) => (false, String::new(), msg),
                };
                commands.extend(app.handle(AppEvent::ExternalEditDone {
                    ok,
                    tmp_path: tmp_path.clone(),
                    text,
                    message,
                }));
                continue;
            }
            execute_one(
                command,
                &client,
                &mut mux,
                &mux_generation,
                &event_tx,
                &mut app,
                &mut commands,
                eff.perf.page_size,
                &config_path,
            )
            .await;
        }
        if app.exited {
            break;
        }

        terminal
            .terminal
            .draw(|frame| dshtui::ui::render(frame, &app))
            .map_err(|e| e.to_string())?;

        if event::poll(Duration::from_millis(eff.ui.tick_ms)).map_err(|e| e.to_string())? {
            let input = event::read().map_err(|e| e.to_string())?;
            let mode = match app.mode {
                Mode::Normal if app.help_open => InputMode::Help,
                Mode::Normal => InputMode::Normal,
                Mode::Picker => InputMode::Picker,
                Mode::Insert => InputMode::Insert,
                // REQ-003 模式机扩展（app.mode 镜像）。
                Mode::Search => InputMode::Search,
                Mode::Visual => InputMode::Visual,
                // REQ-006：审批列表子视图（`L` 打开）用独立键位表（D-036）。
                Mode::Approval if app.approval.list_open => InputMode::ApprovalList,
                Mode::Approval => InputMode::Approval,
                Mode::ImageView => InputMode::ImageView,
                // REQ-005：Trajectory 独立模式（详情子层由 focus 分派，
                // 无独立 InputMode）。
                Mode::Trajectory if app.traj.filter.open => InputMode::TrajectoryFilter,
                Mode::Trajectory => InputMode::Trajectory,
                // REQ-006：模型目录 overlay（`M` 打开；effort 子阶段同键位表）。
                Mode::ModelCatalog => InputMode::ModelCatalog,
                // REQ-006：命令面板 overlay（`:` 打开）。
                Mode::CommandPalette => InputMode::CommandPalette,
                // REQ-007：@ 提及（AC-007-23）。
                Mode::Mention => InputMode::Mention,
                // REQ-007：subagent 目录（FR-007-01）。
                Mode::Subagent => InputMode::Subagent,
                // REQ-007：goal 面板。
                Mode::Goal => InputMode::Goal,
                // REQ-007：jobs 只读面板。
                Mode::Jobs => InputMode::Jobs,
            };
            if let Some(command) = decoder.decode(mode, input) {
                commands.extend(app.handle_command(command));
            }
        }
        while let Ok(event) = event_rx.try_recv() {
            commands.extend(app.handle(event));
        }
        // REQ-007：草稿变更后主循环串行 flush（AC-007-22/ADR-010）。
        if app.take_draft_dirty() {
            flush_drafts(&mut app);
        }
    }
    // REQ-004 退出清理：未入缓存的临时文件（缓存目录由 ImageCache Drop 清理）。
    app.cleanup_transient_files();
    Ok(())
}

/// REQ-007 AC-007-22：把内存草稿注册表 flush 到 drafts.toml（ADR-010 原子
/// 写 0600）。best-effort：失败仅告警回退内存，不崩溃。
fn flush_drafts(app: &mut AppState) {
    let Some(path) = app.drafts_path.clone() else {
        return;
    };
    let store = app.draft_store_snapshot();
    match store.to_toml() {
        Ok(toml_str) => {
            if let Err(e) = dshtui::config::atomic_write_0600(&path, &toml_str) {
                eprintln!("警告: drafts.toml 写入失败（草稿仍保留在内存）: {e}");
            }
        }
        Err(e) => eprintln!("警告: drafts.toml 序列化失败: {e}"),
    }
}

/// REQ-007 AC-007-25（prototype ✅）：前台运行 `$EDITOR <tmp>`（继承 stdio，
/// raw mode 已由 TerminalSession::suspend_for_editor 释放）。错误串可断言。
fn run_editor_blocking(tmp: &std::path::Path, editor: &str) -> Result<(), String> {
    let status = std::process::Command::new(editor)
        .arg(tmp)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("无法启动编辑器 {editor}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "编辑器 {editor} 异常退出（{status}），草稿已保留可重试"
        ))
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_one(
    command: Cmd,
    client: &Option<DshClient>,
    mux: &mut Option<Mux>,
    mux_generation: &Arc<AtomicU64>,
    event_tx: &mpsc::Sender<AppEvent>,
    app: &mut AppState,
    commands: &mut VecDeque<Cmd>,
    page_size: usize,
    config_path: &std::path::Path,
) {
    match command {
        Cmd::LoadSessionList { cursor } => {
            let Some(client) = client.as_ref() else {
                return;
            };
            let event = match session::list(&client.http, &client.base, cursor.as_deref()).await {
                Ok(page) => AppEvent::SessionListPage {
                    items: page.raw_items,
                    next_cursor: page.next_cursor,
                },
                Err(error) => AppEvent::SessionListError(error),
            };
            commands.extend(app.handle(event));
        }
        Cmd::OpenWorkspaceFollow => {
            let Some(client) = client.as_ref() else {
                return;
            };
            let opened = match open_mux_stream(client, mux, mux_generation, event_tx).await {
                Ok(mux_ref) => workspace::open_follow(mux_ref).await,
                Err(error) => Err(error),
            };
            match opened {
                Ok(stream) => {
                    let generation = mux_generation.load(Ordering::Relaxed);
                    spawn_workspace_reader(
                        stream,
                        event_tx.clone(),
                        mux_generation.clone(),
                        generation,
                    );
                }
                Err(error) => {
                    commands.extend(app.handle(AppEvent::Disconnected(error.to_string())));
                }
            }
        }
        Cmd::OpenFollow {
            session_id,
            max_messages,
        } => {
            let Some(client) = client.as_ref() else {
                return;
            };
            let address = SessionAddress::session(&session_id.0);
            let opened = match open_mux_stream(client, mux, mux_generation, event_tx).await {
                Ok(mux_ref) => session::open_follow(mux_ref, &address, max_messages).await,
                Err(error) => Err(error),
            };
            match opened {
                Ok(stream) => {
                    let generation = mux_generation.load(Ordering::Relaxed);
                    spawn_follow_reader(
                        stream,
                        session_id,
                        event_tx.clone(),
                        mux_generation.clone(),
                        generation,
                    );
                }
                Err(error) => {
                    commands.extend(app.handle(AppEvent::FollowError { session_id, error }));
                }
            }
        }
        Cmd::RequestPage {
            session_id,
            generation,
            through_seq,
            before_seq,
            max_messages,
        } => {
            // AppState never issues pages while reconnecting; drop any that
            // were queued before the disconnect (refollow backfills).
            if app.is_reconnecting() {
                return;
            }
            let Some(client) = client.as_ref() else {
                return;
            };
            let result = session::page(
                &client.http,
                &client.base,
                &SessionAddress::session(&session_id.0),
                through_seq,
                before_seq,
                max_messages.min(page_size.max(1)),
            )
            .await;
            let event = match result {
                Ok(page) => AppEvent::PageResult {
                    session_id,
                    generation,
                    records: page.records,
                    has_more: page.has_more,
                },
                Err(error) => AppEvent::PageError {
                    session_id,
                    generation,
                    error,
                },
            };
            commands.extend(app.handle(event));
        }
        Cmd::CancelSession(session_id) => {
            // Best-effort stop (AC-001-08 exit semantics); the result is
            // routed back so the interactive stop path can judge accepted /
            // failure (REQ-002 Step 4).
            let Some(client) = client.as_ref() else {
                return;
            };
            let event = match session::cancel(&client.http, &client.base, &session_id.0).await {
                Ok(accepted) => {
                    tracing::debug!(%session_id, ?accepted, "session/cancel accepted");
                    AppEvent::CancelAccepted {
                        session_id: session_id.clone(),
                    }
                }
                Err(error) => AppEvent::CancelFailed {
                    session_id: session_id.clone(),
                    error,
                },
            };
            commands.extend(app.handle(event));
        }
        Cmd::SendPrompt {
            session_id,
            request,
        } => {
            let Some(client) = client.as_ref() else {
                return;
            };
            let event = match session::prompt(&client.http, &client.base, &request).await {
                Ok(accepted) => {
                    tracing::debug!(%session_id, ?accepted, "session/prompt accepted");
                    AppEvent::PromptAccepted {
                        session_id,
                        request_id: request.request_id.clone(),
                    }
                }
                Err(error) => AppEvent::PromptFailed {
                    session_id,
                    request_id: request.request_id.clone(),
                    error,
                },
            };
            commands.extend(app.handle(event));
        }
        // ---------- REQ-003：搜索 / control / 审批 / 打开 / 跳轮 / 剪贴板 ----------
        Cmd::SearchSessions { query, generation } => {
            let Some(client) = client.as_ref() else {
                return;
            };
            let event =
                match session::search(&client.http, &client.base, &query, SEARCH_TIMEOUT).await {
                    Ok(result) => AppEvent::SearchResult {
                        generation,
                        items: result.items,
                        has_more: result.has_more,
                    },
                    Err(error) => AppEvent::SearchError { generation, error },
                };
            commands.extend(app.handle(event));
        }
        Cmd::DebounceSearch { query, generation } => {
            // 300ms 防抖计时（AC-003-14）：任务只投递事件，generation 校验
            // 在 reducer（模式 15 in-flight 去重，只保留最新）。
            let tx = event_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SEARCH_DEBOUNCE).await;
                let _ = tx
                    .send(AppEvent::SearchHistoryDebounced { query, generation })
                    .await;
            });
        }
        Cmd::OpenControl { session_id } => {
            let Some(client) = client.as_ref() else {
                return;
            };
            let address = SessionAddress::session(&session_id.0);
            let opened = match open_mux_stream(client, mux, mux_generation, event_tx).await {
                Ok(mux_ref) => session::open_control(mux_ref, &address).await,
                Err(error) => Err(error),
            };
            match opened {
                Ok(stream) => {
                    let generation = mux_generation.load(Ordering::Relaxed);
                    spawn_control_reader(
                        stream,
                        session_id,
                        event_tx.clone(),
                        mux_generation.clone(),
                        generation,
                    );
                }
                Err(error) => {
                    tracing::warn!(%session_id, error = %error, "session/control 打开失败");
                    commands.extend(app.handle(AppEvent::FollowError { session_id, error }));
                }
            }
        }
        Cmd::ReplyApproval { event, outcome } => {
            let Some(client) = client.as_ref() else {
                return;
            };
            let result = dshtui::api::approval::reply(
                &client.http,
                &client.base,
                &event,
                outcome,
                dshtui::api::approval::REPLY_TIMEOUT,
            )
            .await;
            let ev = match result {
                Ok(()) => AppEvent::ApprovalReplied { outcome },
                Err(error) => AppEvent::ApprovalReplyFailed { outcome, error },
            };
            commands.extend(app.handle(ev));
        }
        // ---------- REQ-006：模型目录（FR-006-01） ----------
        Cmd::FetchModelCatalog { generation } => {
            let Some(client) = client.as_ref() else {
                // 未连接（重连中/启动失败）：目录区给出可观察错误，不静默
                // （AC-006-06 断网可观察；网络恢复后重新打开/重试即成功）。
                let event = AppEvent::ModelCatalogLoadFailed {
                    generation,
                    error: ClientError::Transport(
                        "未连接（dsh web 不可达），模型目录暂不可用".into(),
                    ),
                };
                commands.extend(app.handle(event));
                return;
            };
            let event = match dshtui::api::session::model_catalog(&client.http, &client.base).await
            {
                Ok(catalog) => AppEvent::ModelCatalogLoaded {
                    generation,
                    catalog,
                },
                Err(error) => AppEvent::ModelCatalogLoadFailed { generation, error },
            };
            commands.extend(app.handle(event));
        }
        Cmd::SelectModel {
            provider,
            model,
            reasoning_effort,
        } => {
            let Some(client) = client.as_ref() else {
                let event = AppEvent::ModelSelectFailed {
                    error: ClientError::Transport("未连接（dsh web 不可达），模型切换失败".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            // 目标会话 = 当前活动会话（模型选择是会话级，next 对该会话生效）。
            // reducer（model_catalog_submit）已 guard 无活动会话不发本命令；
            // 此处兜底仍走 AppEvent 保持单写者（不直改 AppState）。
            let Some(session_id) = app.active_session.clone() else {
                let event = AppEvent::ModelSelectFailed {
                    error: ClientError::Protocol(
                        "无打开的会话：先用 f/o 打开会话再切换模型".into(),
                    ),
                };
                commands.extend(app.handle(event));
                return;
            };
            let event = match dshtui::api::session::select_model(
                &client.http,
                &client.base,
                &session_id,
                &provider,
                &model,
                reasoning_effort.as_deref(),
            )
            .await
            {
                Ok(selected) => AppEvent::ModelSelected { selected },
                Err(error) => AppEvent::ModelSelectFailed { error },
            };
            commands.extend(app.handle(event));
        }
        // ---------- REQ-006：命令面板（FR-006-03） ----------
        Cmd::FetchRemoteCommands => {
            let Some(client) = client.as_ref() else {
                let event = AppEvent::RemoteCommandsFailed {
                    error: ClientError::Transport("未连接（dsh web 不可达）".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            // 官方 wire `agentId: SessionId`（0.1.2-rc.1 实读 dsh-commands
            // .d.ts）：主会话流以活动会话 id 为 agent 作用域。
            let Some(agent_id) = app.active_session.clone() else {
                let event = AppEvent::RemoteCommandsFailed {
                    error: ClientError::Protocol("无活动会话：斜杠命令需先打开会话".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let event =
                match dshtui::api::commands::list(&client.http, &client.base, &agent_id.0).await {
                    Ok(cmds) => AppEvent::RemoteCommandsLoaded { commands: cmds },
                    Err(error) => AppEvent::RemoteCommandsFailed { error },
                };
            commands.extend(app.handle(event));
        }
        Cmd::ExecuteCommand { line } => {
            let Some(client) = client.as_ref() else {
                let event = AppEvent::CommandExecuteFailed {
                    error: ClientError::Transport("未连接（dsh web 不可达）".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let Some(agent_id) = app.active_session.clone() else {
                let event = AppEvent::CommandExecuteFailed {
                    error: ClientError::Protocol("无活动会话：斜杠命令需先打开会话".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let event = match dshtui::api::commands::execute(
                &client.http,
                &client.base,
                &agent_id.0,
                &line,
                &[],
            )
            .await
            {
                Ok(exec) => AppEvent::CommandExecuted {
                    text: exec.and_then(|e| e.result).and_then(|r| r.text),
                },
                Err(error) => AppEvent::CommandExecuteFailed { error },
            };
            commands.extend(app.handle(event));
        }
        // ---------- REQ-006：workspace/session 操作（FR-006-02 操作半） ----------
        Cmd::CreateSession => {
            let Some(client) = client.as_ref() else {
                let event = AppEvent::SessionCreateFailed {
                    error: ClientError::Transport("未连接（dsh web 不可达）".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let event =
                match dshtui::api::session::create(&client.http, &client.base, None, None).await {
                    Ok(created) => AppEvent::SessionCreated {
                        session_id: created.session_id,
                    },
                    Err(error) => AppEvent::SessionCreateFailed { error },
                };
            commands.extend(app.handle(event));
        }
        // ---------- REQ-006：workspace/session 写操作执行（FR-006-02 操作半） ----------
        Cmd::WorkspaceOp { request_id, op } => {
            use dshtui::app::{OpOutcome, WorkspaceOperation};
            let Some(client) = client.as_ref() else {
                let event = AppEvent::WorkspaceOpFailed {
                    request_id,
                    op_name: op.label().to_string(),
                    error: ClientError::Transport("未连接（dsh web 不可达）".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let result: Result<OpOutcome, (String, ClientError)> = match op {
                WorkspaceOperation::ForkSession { session_id } => {
                    dshtui::api::session::fork(&client.http, &client.base, &session_id, None)
                        .await
                        .map(|v| OpOutcome::ForkCreated {
                            session_id: v.session_id,
                        })
                        .map_err(|e| ("fork session".to_string(), e))
                }
                WorkspaceOperation::RenameSession { session_id, title } => {
                    dshtui::api::session::rename(&client.http, &client.base, &session_id, &title)
                        .await
                        .map(|_| OpOutcome::Ack)
                        .map_err(|e| ("rename session".to_string(), e))
                }
                WorkspaceOperation::ArchiveSession { session_id } => {
                    workspace::archive_session(&client.http, &client.base, &session_id.0)
                        .await
                        .map(|_| OpOutcome::Ack)
                        .map_err(|e| ("archive session".to_string(), e))
                }
                WorkspaceOperation::NewWorkspace { path } => {
                    workspace::create_workspace(&client.http, &client.base, &path)
                        .await
                        .map(|raw| {
                            let id = raw
                                .get("workspace")
                                .and_then(|w| {
                                    w.get("workspaceId")
                                        .or_else(|| w.get("id"))
                                        .and_then(|v| v.as_str())
                                })
                                .or_else(|| {
                                    raw.get("workspaceId")
                                        .or_else(|| raw.get("id"))
                                        .and_then(|v| v.as_str())
                                })
                                .unwrap_or("")
                                .to_string();
                            OpOutcome::WorkspaceCreated { workspace_id: id }
                        })
                        .map_err(|e| ("new workspace".to_string(), e))
                }
                WorkspaceOperation::RenameWorkspace {
                    workspace_id,
                    title,
                } => {
                    workspace::rename_workspace(&client.http, &client.base, &workspace_id.0, &title)
                        .await
                        .map(|_| OpOutcome::Ack)
                        .map_err(|e| ("rename workspace".to_string(), e))
                }
                WorkspaceOperation::DeleteWorkspace { workspace_id } => {
                    workspace::delete_workspace(&client.http, &client.base, &workspace_id.0)
                        .await
                        .map(|_| OpOutcome::Ack)
                        .map_err(|e| ("delete workspace".to_string(), e))
                }
                WorkspaceOperation::MoveSession {
                    session_id,
                    target_workspace,
                } => workspace::insert_session_before(
                    &client.http,
                    &client.base,
                    &target_workspace.0,
                    &session_id.0,
                    None,
                )
                .await
                .map(|_| OpOutcome::Ack)
                .map_err(|e| ("move session".to_string(), e)),
            };
            let event = match result {
                Ok(outcome) => AppEvent::WorkspaceOpDone {
                    request_id,
                    outcome,
                },
                Err((op_name, error)) => AppEvent::WorkspaceOpFailed {
                    request_id,
                    op_name,
                    error,
                },
            };
            commands.extend(app.handle(event));
        }
        // ---------- REQ-007：主题切换持久化（AC-007-20/ADR-010） ----------
        // 内联处理：此 arm 不应到达（run_connected 循环已拦截）。
        Cmd::ExternalEdit { .. } => {
            app.last_error = Some("外部编辑器需主循环内联处理".into());
        }
        // ---------- REQ-007：@ 提及两源拉取（AC-007-23） ----------
        // ---------- REQ-007：subagent 目录（FR-007-01，AC-007-07~10） ----------
        Cmd::FetchSubagentList {
            parent_id,
            generation,
        } => {
            let Some(client) = client.as_ref() else {
                let event = AppEvent::SubagentListFailed {
                    parent_id,
                    generation,
                    error: ClientError::Transport("未连接（dsh web 不可达）".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            match dshtui::api::subagents::list(&client.http, &client.base, &parent_id).await {
                Ok(catalog) => commands.extend(app.handle(AppEvent::SubagentListed {
                    parent_id,
                    generation,
                    catalog,
                })),
                Err(error) => commands.extend(app.handle(AppEvent::SubagentListFailed {
                    parent_id,
                    generation,
                    error,
                })),
            }
        }
        // ---------- REQ-007：goal 写操作（FR-007-02，AC-007-11/12/14） ----------
        Cmd::GoalOp { request_id, op } => {
            use dshtui::app::GoalMutation;
            let Some(client) = client.as_ref() else {
                let event = AppEvent::GoalOpFailed {
                    request_id,
                    op,
                    error: ClientError::Transport("未连接（dsh web 不可达）".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let Some(agent_id) = app.active_session.clone() else {
                let event = AppEvent::GoalOpFailed {
                    request_id,
                    op,
                    error: ClientError::Transport("无活动会话".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let result: Result<Option<dshtui::api::types::GoalSnapshot>, (String, ClientError)> =
                match op.clone() {
                    GoalMutation::Create {
                        objective,
                        max_goal_rounds,
                    } => {
                        let req = dshtui::api::types::CreateGoalRequest {
                            objective,
                            max_goal_rounds,
                        };
                        dshtui::api::goals::create(&client.http, &client.base, &agent_id.0, &req)
                            .await
                            .map(|r| {
                                Some(dshtui::api::types::GoalSnapshot {
                                    id: r.id,
                                    revision: r.revision,
                                    ..Default::default()
                                })
                            })
                            .map_err(|e| ("create".into(), e))
                    }
                    GoalMutation::Edit { objective } => {
                        // goals/edit(agentId, ref, request{objective})（typert 实读）。
                        let _ = objective;
                        Err((
                            "edit".into(),
                            ClientError::Protocol("goals/edit 暂未接线".into()),
                        ))
                    }
                    GoalMutation::Pause => {
                        let ref_ = dshtui::api::types::GoalRef {
                            id: app
                                .goals
                                .goal
                                .as_ref()
                                .map(|g| g.id.clone())
                                .unwrap_or_default(),
                            revision: app.goals.sent_revision.unwrap_or(0),
                        };
                        dshtui::api::goals::pause(&client.http, &client.base, &agent_id.0, &ref_)
                            .await
                            .map(Some)
                            .map_err(|e| ("pause".into(), e))
                    }
                    GoalMutation::Resume => {
                        let ref_ = dshtui::api::types::GoalRef {
                            id: app
                                .goals
                                .goal
                                .as_ref()
                                .map(|g| g.id.clone())
                                .unwrap_or_default(),
                            revision: app.goals.sent_revision.unwrap_or(0),
                        };
                        dshtui::api::goals::resume(&client.http, &client.base, &agent_id.0, &ref_)
                            .await
                            .map(Some)
                            .map_err(|e| ("resume".into(), e))
                    }
                    GoalMutation::Complete => {
                        let ref_ = dshtui::api::types::GoalRef {
                            id: app
                                .goals
                                .goal
                                .as_ref()
                                .map(|g| g.id.clone())
                                .unwrap_or_default(),
                            revision: app.goals.sent_revision.unwrap_or(0),
                        };
                        dshtui::api::goals::complete(&client.http, &client.base, &agent_id.0, &ref_)
                            .await
                            .map(Some)
                            .map_err(|e| ("complete".into(), e))
                    }
                    GoalMutation::Clear => {
                        let ref_ = dshtui::api::types::GoalRef {
                            id: app
                                .goals
                                .goal
                                .as_ref()
                                .map(|g| g.id.clone())
                                .unwrap_or_default(),
                            revision: app.goals.sent_revision.unwrap_or(0),
                        };
                        dshtui::api::goals::clear(&client.http, &client.base, &agent_id.0, &ref_)
                            .await
                            .map(|_| None)
                            .map_err(|e| ("clear".into(), e))
                    }
                };
            let cleared = matches!(result, Ok(None));
            match result {
                Ok(updated) => commands.extend(app.handle(AppEvent::GoalOpDone {
                    request_id,
                    updated,
                    cleared,
                })),
                Err((_op_name, error)) => commands.extend(app.handle(AppEvent::GoalOpFailed {
                    request_id,
                    op,
                    error,
                })),
            }
        }
        Cmd::SubagentInterrupt {
            child_id,
            parent_id,
        } => {
            let Some(client) = client.as_ref() else {
                let event = AppEvent::SubagentInterruptDone {
                    child_id,
                    error: Some(ClientError::Transport("未连接（dsh web 不可达）".into())),
                };
                commands.extend(app.handle(event));
                return;
            };
            match dshtui::api::subagents::interrupt_by_parent(
                &client.http,
                &client.base,
                &child_id,
                &parent_id,
            )
            .await
            {
                Ok(_) => commands.extend(app.handle(AppEvent::SubagentInterruptDone {
                    child_id,
                    error: None,
                })),
                Err(error) => commands.extend(app.handle(AppEvent::SubagentInterruptDone {
                    child_id,
                    error: Some(error),
                })),
            }
        }
        Cmd::FetchMentionCandidates {
            generation,
            agent_id,
            query,
        } => {
            let Some(client) = client.as_ref() else {
                let event = AppEvent::MentionCandidatesFailed {
                    generation,
                    error: ClientError::Transport("未连接（dsh web 不可达）".into()),
                };
                commands.extend(app.handle(event));
                return;
            };
            let files = dshtui::api::references::file_references(
                &client.http,
                &client.base,
                &agent_id,
                &query,
            )
            .await;
            let sessions = dshtui::api::references::session_candidates(
                &client.http,
                &client.base,
                &agent_id,
                &query,
            )
            .await;
            // 任一源失败不阻塞另一源；两源皆失败才报错。
            match (files, sessions) {
                (Ok(files), Ok(sessions)) => {
                    commands.extend(app.handle(AppEvent::MentionCandidates {
                        generation,
                        files,
                        sessions,
                    }));
                }
                (Err(fe), Err(se)) => {
                    tracing::warn!(fe = %fe, se = %se, "@ 两源候选拉取均失败");
                    let code = if fe.code() == "transport" { se } else { fe };
                    commands.extend(app.handle(AppEvent::MentionCandidatesFailed {
                        generation,
                        error: code,
                    }));
                }
                (Ok(files), Err(_)) => {
                    commands.extend(app.handle(AppEvent::MentionCandidates {
                        generation,
                        files,
                        sessions: Vec::new(),
                    }));
                }
                (Err(_), Ok(sessions)) => {
                    commands.extend(app.handle(AppEvent::MentionCandidates {
                        generation,
                        files: Vec::new(),
                        sessions,
                    }));
                }
            }
        }
        Cmd::SaveUiTheme { theme, palette } => {
            let result = dshtui::config::save_theme_config(config_path, &theme, &palette);
            match result {
                Ok(()) => {
                    app.notice = Some(format!("主题已保存（重启后仍生效）: {theme}"));
                }
                Err(error) => {
                    app.last_error = Some(format!("主题保存失败: {error}"));
                }
            }
        }
        Cmd::OpenExternal { target } => {
            // 仅用户显式触发才调用系统 open（REQ-003 §3 安全边界）。
            match open::that(&target) {
                Ok(()) => tracing::debug!(target = %target, "系统打开成功"),
                Err(error) => {
                    tracing::warn!(target = %target, error = %error, "系统打开失败");
                    app.last_error = Some(format!("打开失败: {target}（{error}）"));
                }
            }
        }
        Cmd::LoadThrough { seq } => {
            // AC-003-09：每次命令只拉一页（200 条/页口径，Notes/03 §4.4）；
            // 落位判断与续页由 reducer（LoadThroughPage 事件）完成——逐页
            // 之间照常渲染，不阻塞帧循环。分页上限在
            // `AppState::LOAD_THROUGH_MAX_PAGES`。
            let Some(client) = client.as_ref() else {
                return;
            };
            let Some(session_id) = app.active_session.clone() else {
                return;
            };
            let (through_seq, before_seq) = match app.sessions.get(&session_id.0) {
                Some(w) => (
                    w.cursor().map(|c| SessionSeq(c.0)).unwrap_or(SessionSeq(0)),
                    w.head_seq(),
                ),
                None => {
                    tracing::warn!(seq = %seq, "loadThrough 目标会话无窗口，中止");
                    return;
                }
            };
            match session::page(
                &client.http,
                &client.base,
                &SessionAddress::session(&session_id.0),
                through_seq,
                before_seq,
                LOAD_THROUGH_PAGE_SIZE,
            )
            .await
            {
                Ok(page) => {
                    let has_more = page.has_more;
                    commands.extend(app.handle(AppEvent::LoadThroughPage {
                        session_id: session_id.clone(),
                        records: page.records,
                        has_more,
                    }));
                }
                Err(error) => {
                    tracing::warn!(error = %error, "loadThrough 分页失败");
                    app.last_error = Some(format!("跳轮加载失败: {error}"));
                }
            }
        }
        Cmd::CopyToClipboard { text } => {
            // arboard 系统剪贴板 → OSC52 降级链（AC-003-08）；内容不落盘
            // （Notes/06 §9），不进日志明文。
            let outcome = tokio::task::spawn_blocking(move || clipboard_write(text))
                .await
                .unwrap_or((dshtui::model::YankBackend::Unavailable, false));
            commands.extend(app.handle(AppEvent::CopyDone {
                backend: outcome.0,
                ok: outcome.1,
            }));
        }
        Cmd::FetchAttachment {
            session_id,
            attachment_id,
            block_seq,
            for_viewer,
        } => {
            // Keep the frame loop responsive while the remote unary request is in flight.
            let Some(client) = client.as_ref() else {
                // 未连接：清掉在途标记并走既有重连（AC-004-06/08 恢复路径），
                // 否则该图会永久“在途”导致重连后无法重开。
                let event = AppEvent::AttachmentFailed {
                    session_id,
                    attachment_id,
                    block_seq,
                    code: "network".into(),
                    message: "附件拉取时连接未就绪".into(),
                    retryable: true,
                    for_viewer,
                };
                commands.extend(app.handle(event));
                return;
            };
            let http = client.http.clone();
            let base = client.base.clone();
            let cache = std::sync::Arc::clone(&app.image_cache);
            let kitty = app.kitty_capable && !for_viewer;
            let font_size = dshtui::ui::image::terminal_font_size();
            let area = encode_area(app);
            let frame_id = app.next_kitty_frame_id();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let fetched =
                    dshtui::api::attachment::fetch(&http, &base, &session_id, &attachment_id).await;
                // Decode/downsample/Kitty encoding runs off the async executor.
                let _ = tokio::task::spawn_blocking::<_, ()>(move || {
                    let event = match fetched {
                        Ok(data) => {
                            let media = data.media_type.clone();
                            let meta = dshtui::model::AttachmentRef::from(&data);
                            match dshtui::ui::image::decode_image(&data.image_bytes, &media) {
                                Ok(decoded) => {
                                    let temp_file = match cache
                                        .write_temp_file(&media, data.image_bytes.clone())
                                    {
                                        Ok(p) => p,
                                        Err(e) => {
                                            tracing::error!(error = %e, "附件临时文件写入失败");
                                            let _ = event_tx.blocking_send(AppEvent::AttachmentFailed {
                                                session_id,
                                                attachment_id,
                                                block_seq,
                                                code: "io".into(),
                                                message: format!("临时文件写入失败: {e}"),
                                                retryable: false,
                                                for_viewer,
                                            });
                                            return;
                                        }
                                    };
                                    let entry = dshtui::model::ImageCacheEntry {
                                        attachment_id: attachment_id.clone(),
                                        media_type: media.clone(),
                                        bytes: data.image_bytes.len() as u64,
                                        width: decoded.width as u64,
                                        height: decoded.height as u64,
                                        temp_file: temp_file.clone(),
                                        last_used: 0,
                                    };
                                    if !kitty {
                                        let _ = event_tx.blocking_send(AppEvent::AttachmentReady {
                                            session_id,
                                            attachment_id,
                                            block_seq,
                                            meta,
                                            frame: None,
                                            entry,
                                            cached: false,
                                            for_viewer,
                                        });
                                        return;
                                    }
                                    match dshtui::ui::image::kitty_frame(
                                        image::DynamicImage::ImageRgba8(decoded.rgba),
                                        font_size,
                                        area,
                                        frame_id,
                                    ) {
                                        Ok(f) => event_tx.blocking_send(AppEvent::AttachmentReady {
                                            session_id,
                                            attachment_id,
                                            block_seq,
                                            meta,
                                            frame: Some(dshtui::app::KittyFrame(f)),
                                            entry,
                                            cached: false,
                                            for_viewer,
                                        }),
                                        Err(e) => {
                                            let _ = std::fs::remove_file(&temp_file);
                                            tracing::error!(code = %e.code, error = %e.message, "Kitty 图片帧编码失败");
                                            event_tx.blocking_send(AppEvent::AttachmentFailed {
                                                session_id,
                                                attachment_id,
                                                block_seq,
                                                code: e.code,
                                                message: e.message,
                                                retryable: false,
                                                for_viewer,
                                            })
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(code = %e.code, error = %e.message, "附件图片解码失败");
                                    event_tx.blocking_send(AppEvent::AttachmentFailed {
                                        session_id,
                                        attachment_id,
                                        block_seq,
                                        code: e.code,
                                        message: e.message,
                                        retryable: false,
                                        for_viewer,
                                    })
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "附件远程拉取失败");
                            let retryable = e.class() == dshtui::api::ErrorClass::Retryable;
                            let code = match &e {
                                dshtui::api::ClientError::Remote { code, .. } => code.clone(),
                                _ if retryable => "network".into(),
                                _ => "attachment".into(),
                            };
                            event_tx.blocking_send(AppEvent::AttachmentFailed {
                                session_id,
                                attachment_id,
                                block_seq,
                                code,
                                message: e.to_string(),
                                retryable,
                                for_viewer,
                            })
                        }
                    };
                    let _ = event;
                })
                .await;
            });
        }
        Cmd::RenderCachedImage {
            session_id,
            attachment_id,
            block_seq,
            temp_file,
            media_type,
        } => {
            // REQ-004 缓存命中：从临时文件重解码 + kitty 编码（不重复拉取）。
            let meta = app.image_meta.get(&attachment_id).cloned();
            let font_size = dshtui::ui::image::terminal_font_size();
            let area = encode_area(app);
            let frame_id = app.next_kitty_frame_id();
            let cache = std::sync::Arc::clone(&app.image_cache);
            let event_tx = event_tx.clone();
            tokio::task::spawn_blocking::<_, ()>(move || {
                let event = match std::fs::read(&temp_file) {
                    Ok(bytes) => match dshtui::ui::image::decode_image(&bytes, &media_type) {
                        Ok(decoded) => {
                            let meta = meta.unwrap_or_else(|| dshtui::model::AttachmentRef {
                                attachment_id: attachment_id.clone(),
                                media_type: media_type.clone(),
                                bytes: bytes.len() as u64,
                                width: decoded.width as u64,
                                height: decoded.height as u64,
                                name: None,
                                original_dimensions: None,
                            });
                            let entry = match cache.get(&attachment_id) {
                                Some(e) => e,
                                None => dshtui::model::ImageCacheEntry {
                                    attachment_id: attachment_id.clone(),
                                    media_type: media_type.clone(),
                                    bytes: bytes.len() as u64,
                                    width: decoded.width as u64,
                                    height: decoded.height as u64,
                                    temp_file: temp_file.clone(),
                                    last_used: 0,
                                },
                            };
                            match dshtui::ui::image::kitty_frame(
                                image::DynamicImage::ImageRgba8(decoded.rgba),
                                font_size,
                                area,
                                frame_id,
                            ) {
                                Ok(f) => AppEvent::AttachmentReady {
                                    session_id,
                                    attachment_id,
                                    block_seq,
                                    meta,
                                    frame: Some(dshtui::app::KittyFrame(f)),
                                    entry,
                                    cached: true,
                                    for_viewer: false,
                                },
                                Err(e) => AppEvent::AttachmentFailed {
                                    session_id,
                                    attachment_id,
                                    block_seq,
                                    code: e.code,
                                    message: e.message,
                                    retryable: false,
                                    for_viewer: false,
                                },
                            }
                        }
                        Err(e) => AppEvent::AttachmentFailed {
                            session_id,
                            attachment_id,
                            block_seq,
                            code: e.code,
                            message: e.message,
                            retryable: false,
                            for_viewer: false,
                        },
                    },
                    Err(e) => AppEvent::AttachmentFailed {
                        session_id,
                        attachment_id,
                        block_seq,
                        code: "io".into(),
                        message: format!("缓存文件读取失败: {e}"),
                        retryable: false,
                        for_viewer: false,
                    },
                };
                let _ = event_tx.blocking_send(event);
            });
        }
        Cmd::OpenSystemViewer { path } => {
            // AC-004-05/07：`open`/`xdg-open` 子进程不阻塞（spawn 不 wait）。
            if !path.exists() {
                app.last_error = Some("图片文件不存在，无法打开".into());
                return;
            }
            let spawned = if cfg!(target_os = "macos") {
                std::process::Command::new("open").arg(&path).spawn()
            } else {
                std::process::Command::new("xdg-open").arg(&path).spawn()
            };
            if let Err(e) = spawned {
                app.last_error = Some(format!("系统查看器打开失败: {e}"));
            }
        }
        Cmd::CopyImageText { text } => {
            // AC-004-05 `y`：复用 REQ-003 降级链（arboard → OSC52 → tmux
            // buffer，§4 复制口径），结果走 reducer 事件（单一状态变更 seam）。
            let outcome = tokio::task::spawn_blocking(move || clipboard_write(text))
                .await
                .unwrap_or((dshtui::model::YankBackend::Unavailable, false));
            commands.extend(app.handle(AppEvent::CopyDone {
                backend: outcome.0,
                ok: outcome.1,
            }));
        }
        Cmd::Reconnect { .. } => {
            // Handled at the top of the run loop so the UI keeps painting.
            commands.push_front(command);
        }
        Cmd::RestoreTerminal => {}
        Cmd::Exit => app.exited = true,
    }
}

/// 编码时的视口区域（与 `ui::split` 同一 seam：ImageView 中心区尺寸）。
fn encode_area(app: &AppState) -> ratatui::layout::Rect {
    let full = ratatui::layout::Rect::new(0, 0, app.width, app.height);
    dshtui::ui::split(
        full,
        app.focus == dshtui::app::Focus::Details,
        app.details_width_cells,
    )
    .center
}

/// Open a stream on the shared mux, creating the mux connection first if needed.
/// On creation the server-push subscription is spawned (REQ-003 approval
/// bypass): it dies with the mux, so a reconnect naturally re-subscribes.
async fn open_mux_stream<'a>(
    client: &DshClient,
    mux: &'a mut Option<Mux>,
    mux_generation: &Arc<AtomicU64>,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<&'a Mux, ClientError> {
    if mux.is_none() {
        let opened = client.open_mux().await?;
        let generation = mux_generation.fetch_add(1, Ordering::Relaxed) + 1;
        spawn_push_reader(
            opened.subscribe_push(),
            event_tx.clone(),
            mux_generation.clone(),
            generation,
        );
        *mux = Some(opened);
    }
    Ok(mux.as_ref().expect("mux initialized"))
}

fn spawn_workspace_reader(
    mut stream: dshtui::api::StreamHandle,
    event_tx: mpsc::Sender<AppEvent>,
    mux_generation: Arc<AtomicU64>,
    generation: u64,
) {
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                Ok(frame) => {
                    let _ = event_tx.send(AppEvent::WorkspaceFrame(frame)).await;
                }
                Err(error) => {
                    if mux_generation.load(Ordering::Relaxed) == generation {
                        let _ = event_tx
                            .send(AppEvent::Disconnected(error.to_string()))
                            .await;
                    }
                    break;
                }
            }
        }
    });
}

/// mux 推帧旁路订阅（REQ-003：无 streamId 的 approval/request waterfall 事件）。
/// 订阅随 mux 生命周期结束；generation 守卫丢弃旧 mux 的迟到帧（模式 15）。
fn spawn_push_reader(
    mut push: tokio::sync::broadcast::Receiver<serde_json::Value>,
    event_tx: mpsc::Sender<AppEvent>,
    mux_generation: Arc<AtomicU64>,
    generation: u64,
) {
    tokio::spawn(async move {
        loop {
            match push.recv().await {
                Ok(raw) => {
                    if mux_generation.load(Ordering::Relaxed) != generation {
                        break;
                    }
                    if let Some(event) = dshtui::api::approval::parse_event(&raw) {
                        tracing::debug!(event_id = %event.event_id, "审批事件经推帧旁路到达");
                        let _ = event_tx.send(AppEvent::ApprovalRequest { event }).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// `session/control` 流 reader：baseline/替换帧解析在 api 层；流错误在
/// generation 未变时统一走断线编排（与 follow 同口径）。
fn spawn_control_reader(
    mut stream: dshtui::api::StreamHandle,
    session_id: SessionId,
    event_tx: mpsc::Sender<AppEvent>,
    mux_generation: Arc<AtomicU64>,
    generation: u64,
) {
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                Ok(value) => {
                    if let Some(item) = session::parse_control_item(&value) {
                        let _ = event_tx
                            .send(AppEvent::ControlItem {
                                session_id: session_id.clone(),
                                item,
                            })
                            .await;
                    }
                }
                Err(error) => {
                    if mux_generation.load(Ordering::Relaxed) == generation {
                        let _ = event_tx
                            .send(AppEvent::Disconnected(error.to_string()))
                            .await;
                    }
                    break;
                }
            }
        }
    });
}

/// 剪贴板写入降级链（AC-003-08）：arboard（系统剪贴板）→ OSC52（终端转义）
/// → 失败提示。仅用户主动复制时输出转义序列（REQ-003 §7）；内容不落盘。
fn clipboard_write(text: String) -> (dshtui::model::YankBackend, bool) {
    // arboard（系统剪贴板）→ OSC52（终端转义）→ tmux buffer → 失败
    // （AC-003-08 降级链，§5 字段表）。
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        if clipboard.set_text(text.clone()).is_ok() {
            return (dshtui::model::YankBackend::System, true);
        }
    }
    let escape = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = io::stdout();
    if writeln!(out, "{escape}").is_ok() && out.flush().is_ok() {
        return (dshtui::model::YankBackend::Osc52, true);
    }
    match std::process::Command::new("tmux")
        .args(["load-buffer", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            let mut stdin = child
                .stdin
                .take()
                .ok_or(std::io::Error::other("no stdin"))?;
            stdin.write_all(text.as_bytes())?;
            drop(stdin);
            child.wait().map(|status| status.success())
        }) {
        Ok(true) => (dshtui::model::YankBackend::Tmux, true),
        _ => (dshtui::model::YankBackend::Unavailable, false),
    }
}

/// RFC 4648 标准字母表 base64（OSC52 需要；不引入新依赖）。
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn spawn_follow_reader(
    mut stream: dshtui::api::StreamHandle,
    session_id: SessionId,
    event_tx: mpsc::Sender<AppEvent>,
    mux_generation: Arc<AtomicU64>,
    generation: u64,
) {
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                Ok(value) => {
                    // Follow frame parsing lives in api/session (protocol
                    // knowledge centralized in the api layer).
                    match session::parse_follow_item(&value) {
                        Some(session::FollowItem::Snapshot {
                            cursor,
                            records,
                            has_more,
                            projections,
                        }) => {
                            let _ = event_tx
                                .send(AppEvent::FollowSnapshot {
                                    session_id: session_id.clone(),
                                    cursor,
                                    records,
                                    has_more,
                                    projections,
                                })
                                .await;
                        }
                        Some(session::FollowItem::Event(event)) => {
                            let _ = event_tx
                                .send(AppEvent::FollowEvent {
                                    session_id: session_id.clone(),
                                    event,
                                })
                                .await;
                        }
                        Some(session::FollowItem::Chunks(row)) => {
                            let _ = event_tx
                                .send(AppEvent::FollowChunks {
                                    session_id: session_id.clone(),
                                    row,
                                })
                                .await;
                        }
                        None => {}
                    }
                }
                Err(error) => {
                    if mux_generation.load(Ordering::Relaxed) == generation {
                        let _ = event_tx
                            .send(AppEvent::FollowError {
                                session_id: session_id.clone(),
                                error,
                            })
                            .await;
                    }
                    break;
                }
            }
        }
    });
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        Ok(Self {
            terminal: Terminal::new(backend)?,
        })
    }

    /// `:edit`（AC-007-25，prototype ✅）：释放 raw mode + 退出 alt-screen，
    /// 让前台 `$EDITOR` 可交互；与 `enter()` 完全互逆，无需新终端框架。
    fn suspend_for_editor(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// 编辑器退出后恢复 raw mode + 重进 alt-screen（与挂起前一致）。
    fn resume_from_editor(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen)?;
        self.terminal.clear()?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_rfc4648_for_osc52() {
        // AC-003-08：OSC52 降级路径的标准 base64（含 padding）。
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b"dshtui"), "ZHNodHVp");
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn clipboard_write_never_panics_ac003_08() {
        // 无显示服务器环境：arboard 失败 → OSC52 转义输出 → tmux buffer →
        // Unavailable；任何环境组合都不崩溃（AC-003-08 应用不崩溃）。
        let (backend, ok) = clipboard_write("test-yank".into());
        assert!(!ok || backend != dshtui::model::YankBackend::Unavailable);
        assert!(matches!(
            backend,
            dshtui::model::YankBackend::System
                | dshtui::model::YankBackend::Osc52
                | dshtui::model::YankBackend::Tmux
                | dshtui::model::YankBackend::Unavailable
        ));
    }

    #[test]
    fn rotating_file_moves_oversized_log_to_old_and_resets_counter() {
        let dir = std::env::temp_dir().join(format!("dshtui-rot-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dshtui.log");
        {
            let mut writer = RotatingFile::new(&path).unwrap();
            // Drive one rotation with a small write past the limit.
            let chunk = vec![b'x'; 1_000];
            let mut total = 0usize;
            while total < 6 * 1024 * 1024 {
                let n = writer.write(&chunk).unwrap();
                assert_eq!(n, chunk.len());
                total += n;
            }
            writer.flush().unwrap();
            assert!(
                writer.written < LOG_ROTATE_BYTES,
                "counter resets after rotation"
            );
        }
        // The pre-rotation content lives in <name>.old and a fresh log exists.
        let old = path.with_extension("log.old");
        assert!(old.exists(), "rotated file preserved as .old");
        assert!(path.exists(), "fresh log file recreated");
        let fresh_len = fs::metadata(&path).unwrap().len();
        assert!(fresh_len < LOG_ROTATE_BYTES);
        // Cleanup: remove only the temp directory this test created.
        let _ = fs::remove_dir_all(&dir);
    }
}
