//! dshtui entry point: configuration, remote connection, and the terminal
//! event loop. The client never starts `dsh web`; connection failures stay
//! visible and actionable (AC-001-02).

use std::collections::VecDeque;
use std::io::{self, Stdout};
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
use dshtui::api::types::{FollowFrame, SessionAddress, SessionHistoryRecord, SessionId};
use dshtui::api::workspace;
use dshtui::api::{Backoff, ClientError, DshClient, Mux};
use dshtui::app::{AppEvent, AppState, Cmd, Mode};
use dshtui::config::{self, Cli, CliAction, Effective};
use dshtui::input::{InputMode, KeyDecoder};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

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

    let Some(token) = eff.token.clone() else {
        eprintln!(
            "错误: 未找到 token。请设置 DSH_TOKEN（或使用 --token <环境变量名>）；secret 不应直接放入命令行。"
        );
        return ExitCode::from(2);
    };

    match run_remote(eff, token).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::from(1)
        }
    }
}

/// Startup probe (ADR-001: never spawn the backend). A failed probe enters the
/// AC-001-02 guidance screen; `r` re-probes, `q`/`Ctrl+c` exits.
async fn run_remote(eff: Effective, token: String) -> Result<(), String> {
    let client = match DshClient::connect(&eff.url, &token).await {
        Ok(client) => client,
        Err(error) => return run_startup_guidance(&eff, &token, error).await,
    };
    run_connected(eff, token, client).await
}

async fn run_startup_guidance(
    eff: &Effective,
    token: &str,
    error: ClientError,
) -> Result<(), String> {
    let mut app = AppState::new(eff.perf.window_messages);
    app.handle(AppEvent::StartupProbeFailed(error.to_string()));
    let mut terminal = TerminalSession::enter().map_err(|e| e.to_string())?;
    let mut decoder = KeyDecoder::new();
    loop {
        terminal
            .terminal
            .draw(|frame| dshtui::ui::render(frame, &app))
            .map_err(|e| e.to_string())?;
        if app.exited {
            return Ok(());
        }
        if event::poll(Duration::from_millis(eff.ui.tick_ms)).map_err(|e| e.to_string())? {
            let input = event::read().map_err(|e| e.to_string())?;
            if let Some(command) = decoder.decode(InputMode::Normal, input) {
                if matches!(command, dshtui::input::Command::RetryProbe) {
                    app.handle_command(command);
                    match DshClient::connect(&eff.url, token).await {
                        Ok(client) => {
                            return run_connected(eff.clone(), token.to_string(), client).await
                        }
                        Err(error) => {
                            app.handle(AppEvent::StartupProbeFailed(error.to_string()));
                        }
                    }
                } else {
                    app.handle_command(command);
                }
            }
        }
    }
}

/// Main loop: single AppState, command queue, mpsc async events, one terminal.
/// Reconnect is handled at the top of the loop so the "reconnecting" status bar
/// keeps painting during the backoff sleep (AC-001-05).
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

        execute_commands(
            &client,
            &mut mux,
            &mux_generation,
            &event_tx,
            &mut app,
            &mut commands,
            eff.perf.page_size,
        )
        .await;
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
async fn execute_commands(
    client: &Option<DshClient>,
    mux: &mut Option<Mux>,
    mux_generation: &Arc<AtomicU64>,
    event_tx: &mpsc::Sender<AppEvent>,
    app: &mut AppState,
    commands: &mut VecDeque<Cmd>,
    page_size: usize,
) {
    while let Some(command) = commands.pop_front() {
        match command {
            Cmd::LoadSessionList { cursor } => {
                let Some(client) = client.as_ref() else {
                    continue;
                };
                let event = match session::list(&client.http, &client.base, cursor.as_deref()).await
                {
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
                    continue;
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
                    continue;
                };
                let address = SessionAddress::session(&session_id.0);
                let opened = match open_mux_stream(client, mux, mux_generation).await {
                    Ok(stream) => session::open_follow(stream, &address, max_messages).await,
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
                    continue;
                }
                let Some(client) = client.as_ref() else {
                    continue;
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
                // Best-effort stop before exit (AC-001-08); never blocks exit.
                if let Some(client) = client.as_ref() {
                    let _ = session::cancel(&client.http, &client.base, &session_id.0).await;
                }
            }
            Cmd::Reconnect { .. } => {
                // Handled at the top of the run loop so the UI keeps painting.
                commands.push_front(command);
                return;
            }
            Cmd::RestoreTerminal => {}
            Cmd::Exit => app.exited = true,
        }
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
                    if let Ok(frame) = serde_json::from_value::<FollowFrame>(value.clone()) {
                        match frame {
                            FollowFrame::Snapshot {
                                cursor,
                                records,
                                has_more,
                                projections,
                                ..
                            } => {
                                let _ = event_tx
                                    .send(AppEvent::FollowSnapshot {
                                        session_id: session_id.clone(),
                                        cursor,
                                        records,
                                        has_more: has_more.unwrap_or(false),
                                        projections,
                                    })
                                    .await;
                            }
                            FollowFrame::Event { event } => {
                                let _ = event_tx
                                    .send(AppEvent::FollowEvent {
                                        session_id: session_id.clone(),
                                        event,
                                    })
                                    .await;
                            }
                        }
                    } else if let Ok(SessionHistoryRecord::Chunks { event: row }) =
                        serde_json::from_value::<SessionHistoryRecord>(value)
                    {
                        let _ = event_tx
                            .send(AppEvent::FollowChunks {
                                session_id: session_id.clone(),
                                row,
                            })
                            .await;
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
