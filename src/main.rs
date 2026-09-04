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
use dshtui::api::types::{SessionAddress, SessionId};
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    let mut decoder = KeyDecoder::new();
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
    let mut terminal = TerminalSession::enter().map_err(|e| e.to_string())?;
    let mut app = AppState::new(eff.perf.window_messages);
    let mut decoder = KeyDecoder::new();
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
            execute_one(
                command,
                &client,
                &mut mux,
                &mux_generation,
                &event_tx,
                &mut app,
                &mut commands,
                eff.perf.page_size,
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
            };
            if let Some(command) = decoder.decode(mode, input) {
                commands.extend(app.handle_command(command));
            }
        }
        while let Ok(event) = event_rx.try_recv() {
            commands.extend(app.handle(event));
        }
    }
    Ok(())
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
            let opened = match open_mux_stream(client, mux, mux_generation).await {
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
            let opened = match open_mux_stream(client, mux, mux_generation).await {
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
        Cmd::Reconnect { .. } => {
            // Handled at the top of the run loop so the UI keeps painting.
            commands.push_front(command);
        }
        Cmd::RestoreTerminal => {}
        Cmd::Exit => app.exited = true,
    }
}

/// Open a stream on the shared mux, creating the mux connection first if needed.
async fn open_mux_stream<'a>(
    client: &DshClient,
    mux: &'a mut Option<Mux>,
    mux_generation: &Arc<AtomicU64>,
) -> Result<&'a Mux, ClientError> {
    if mux.is_none() {
        *mux = Some(client.open_mux().await?);
        mux_generation.fetch_add(1, Ordering::Relaxed);
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
