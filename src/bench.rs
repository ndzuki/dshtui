//! 性能基准 harness（REQ-008 FR-008-01 / AC-008-01~08；D-56）。
//!
//! 形态（Step 3 设计对比采纳 A + 吸收 C/B）：单一 lib 入口
//! `dshtui bench [--report PATH] [--fixture auto|live|seed] [--scenario NAME]`
//! —— 默认零 flag 全量跑 **seed** 场景（确定性、无网络），退出码 0=全 PASS /
//! 1=有 FAIL / 2=under-scale 全 skip。产物为固定 schema 的 PerfReport JSON
//! （原子写 `target/perf/perf-report.json`）。
//!
//! 场景全部为**进程内**测量（被测进程即 `dshtui bench` 自身；release 构建下
//! RSS 与真实 TUI 进程同构——「RSS 单进程诚实测量」口径，Notes/06 §8），
//! 渲染一律走 `ratatui::backend::TestBackend`（headless，不开终端、不发网络）。
//! live fixture 只是本步预留的 scale-guard：live/auto 需要本机 3080 只读可达
//! （`probe_live_available` 仅 TCP connect），不可达时整份报告标 skip（exit 2），
//! 不误杀、不失败。
//!
//! 阈值表集中在 `THRESHOLDS`（一 AC 指标一行），`completeness` 单测锁死表↔
//! 场景不漂移；seed fixture 确定性（固定 id/标题枚举、无 RNG、无时钟字段）
//! 保证双跑一致。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};

use crate::api::types::{
    AttachmentId, ChunkData, ChunkRow, MediaType, SessionHistoryRecord, SessionId, SessionMeta,
    SessionSeq, SessionWireEvent,
};
use crate::app::{AppEvent, AppState, ConnState, Focus, Mode};
use crate::model::{CatalogIndex, ImageCacheEntry};

/// 报告 schema 版本（产物格式固定，D-56）。
pub const REPORT_SCHEMA_VERSION: u32 = 1;
/// 默认报告路径（相对运行目录；原子写同目录 tmp+rename）。
pub const DEFAULT_REPORT_PATH: &str = "target/perf/perf-report.json";

/// seed fixture 规模常量（REQ-008 AC-008 口径：1042 会话 / 1000 模型 /
/// 27turn·1144step 长会话 / 200 滚动采样帧）。
const SEED_SESSIONS: usize = 1042;
const CATALOG_ITEMS: usize = 1000;
/// 27 turn / 1144 step 合计（均摊到各 turn，确定性无 RNG）。
const SCROLL_TURNS: u64 = 27;
const SCROLL_STEPS: usize = 1144;
/// 长会话窗口容量：容纳 fixture 尾部足够多的真实块参与逐帧 layout。
const SCROLL_WINDOW_CAP: usize = 1200;
const SCROLL_FRAMES: usize = 200;
/// 图片缓存预算（= config `[perf] cache_bytes` 默认 32MB；合成账本填满）。
const IMAGE_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// 单合成图片条目记账字节（64KB × 512 次尝试，LRU 会按预算驱逐至 ~32MB）。
const IMAGE_ENTRY_BYTES: u64 = 64 * 1024;

/// fixture 模式（--fixture）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureMode {
    /// 探测本机 3080 只读可达则 live；不可达 → under-scale 全 skip（exit 2）。
    Auto,
    /// live 只读（需要 3080 可达；不可达 → 全 skip，不误杀）。Step 3 尚无
    /// server-backed 数据场景，可达时按进程内场景测量。
    Live,
    /// 确定性 seed（默认；无网络依赖，CI 可复现）。
    Seed,
}

impl FixtureMode {
    /// `--fixture` 字符串解析（非法值返回中文错误，由 CLI 层映射 exit 2）。
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(FixtureMode::Auto),
            "live" => Ok(FixtureMode::Live),
            "seed" => Ok(FixtureMode::Seed),
            other => Err(format!("未知 --fixture `{other}`（可选 auto|live|seed）")),
        }
    }
}

/// 单个场景的度量行（seed 场景恒定；under-scale 时整份报告走 skip）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scenario {
    name: &'static str,
    /// 测得值（ms 或 MB）；None = 该次未测（skip）。
    measured: Option<f64>,
}

/// 阈值表：AC-008 指标一行（Notes/06 §7-8 + charter §3.2 + Notes/09 O01~O05）。
/// `max` 为达标上界；`unit` 展示用。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Threshold {
    pub metric: &'static str,
    pub max: f64,
    pub unit: &'static str,
}

/// 基准阈值（全部以 ms / MB 计，越小越好）。
pub const THRESHOLDS: &[Threshold] = &[
    // AC-008-03：启动到可交互 <1s、列表首屏 <300ms、picker/搜索 <30ms。
    Threshold {
        metric: "startup_ms",
        max: 1000.0,
        unit: "ms",
    },
    Threshold {
        metric: "first_screen_ms",
        max: 300.0,
        unit: "ms",
    },
    Threshold {
        metric: "search_ms",
        max: 30.0,
        unit: "ms",
    },
    // AC-008-04：滚动帧 p99 <33ms；AC-008-06 scale 10k 不劣化。
    Threshold {
        metric: "scroll_frame_p99_ms",
        max: 33.0,
        unit: "ms",
    },
    // AC-008-02：RSS（空闲 <25MB / 流式 <80MB / 图片密集 <150MB，LRU 受控）。
    Threshold {
        metric: "idle_rss_mb",
        max: 25.0,
        unit: "MB",
    },
    Threshold {
        metric: "stream_rss_mb",
        max: 80.0,
        unit: "MB",
    },
    Threshold {
        metric: "image_rss_mb",
        max: 150.0,
        unit: "MB",
    },
];

/// 单指标结果（报告行；`measured` 缺省 = skip，不判成败）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricResult {
    pub metric: String,
    pub threshold: f64,
    pub unit: String,
    /// 实测值（ms 或 MB）；None = 未测（status=skip）。
    #[serde(default)]
    pub measured: Option<f64>,
    /// 仅 `measured` 存在时有意义（`measured ≤ threshold`）。
    pub pass: bool,
    /// pass | fail | skip（skip = 环境/under-scale，不判成败）。
    pub status: String,
}

/// 报告运行元信息（fixture 规模/时间/版本）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunMetadata {
    pub fixture: String,
    pub dshtui_version: String,
    pub started_at_ms: i64,
    pub duration_ms: u64,
    /// 本次实际 seed 的会话数（first_screen 场景规模；live 缺数据时为 0）。
    pub seed_sessions: usize,
}

/// PerfReport 固定 JSON schema（D-56）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerfReport {
    pub schema_version: u32,
    pub dshtui_version: String,
    pub run_metadata: RunMetadata,
    pub results: Vec<MetricResult>,
    /// 全 PASS / N 项 FAIL / N 项 skip 摘要。
    pub summary: String,
}

/// `dshtui bench` 运行配置。
#[derive(Debug, Clone, PartialEq)]
pub struct BenchConfig {
    /// 报告路径；空串 = 不写盘只返回（供单测 / `--report ""`）。
    pub report_path: PathBuf,
    pub fixture: FixtureMode,
    /// 单场景过滤（None = 全量）；非 gate。
    pub scenario: Option<String>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            report_path: PathBuf::from(DEFAULT_REPORT_PATH),
            // REQ-008 Step 3：默认 seed，不依赖本机 3080。
            fixture: FixtureMode::Seed,
            scenario: None,
        }
    }
}

/// 场景规模参数（run() 用全量；单测用小规模保证 debug 下也秒级完成）。
#[derive(Debug, Clone, Copy)]
struct Scale {
    seed_sessions: usize,
    catalog_items: usize,
    turns: u64,
    steps: usize,
    window_cap: usize,
    scroll_frames: usize,
}

impl Scale {
    fn full() -> Self {
        Self {
            seed_sessions: SEED_SESSIONS,
            catalog_items: CATALOG_ITEMS,
            turns: SCROLL_TURNS,
            steps: SCROLL_STEPS,
            window_cap: SCROLL_WINDOW_CAP,
            scroll_frames: SCROLL_FRAMES,
        }
    }
}

/// 运行基准（全量规模），返回报告 + 退出码（0 全 PASS / 1 有 FAIL /
/// 2 under-scale 全 skip）。
pub fn run(cfg: &BenchConfig) -> (PerfReport, i32) {
    run_scaled(cfg, Scale::full())
}

/// run 的可测内核：fixture gate 后测量 → 阈值比对 → 报告 + 退出码。
fn run_scaled(cfg: &BenchConfig, scale: Scale) -> (PerfReport, i32) {
    let started = Instant::now();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // fixture gate：seed 恒测量；live/auto 需 3080 只读可达（仅 TCP connect），
    // 不可达 → 整份报告 skip（under-scale，exit 2，不误杀）。
    let (fixture, scenarios) = match cfg.fixture {
        FixtureMode::Seed => ("seed", measure_scenarios(cfg.scenario.as_deref(), &scale)),
        FixtureMode::Live => {
            if probe_live_available() {
                ("live", measure_scenarios(cfg.scenario.as_deref(), &scale))
            } else {
                ("live", Vec::new())
            }
        }
        FixtureMode::Auto => {
            if probe_live_available() {
                ("live", measure_scenarios(cfg.scenario.as_deref(), &scale))
            } else {
                ("auto", Vec::new())
            }
        }
    };
    let seed_sessions = if fixture == "seed" || fixture == "live" {
        // live 在 Step 3 也没有 server-backed 数据，仍以确定性 seed 会话为
        // 列表规模口径（诚实标注实际 seed 数）。
        measure_seed_sessions(cfg.scenario.as_deref(), scale)
    } else {
        0
    };

    // 阈值 → 报告行（每 AC 指标一行；场景测得 None → status=skip）。
    let mut results: Vec<MetricResult> = Vec::with_capacity(THRESHOLDS.len());
    for t in THRESHOLDS {
        let measured = scenarios
            .iter()
            .find(|s| s.name == t.metric)
            .and_then(|s| s.measured);
        match measured {
            Some(v) => {
                let pass = v <= t.max;
                results.push(MetricResult {
                    metric: t.metric.to_string(),
                    threshold: t.max,
                    unit: t.unit.to_string(),
                    measured: Some(v),
                    pass,
                    status: if pass { "pass" } else { "fail" }.to_string(),
                });
            }
            None => results.push(MetricResult {
                metric: t.metric.to_string(),
                threshold: t.max,
                unit: t.unit.to_string(),
                measured: None,
                pass: false,
                status: "skip".to_string(),
            }),
        }
    }

    let summary = summarize(&results);

    let report = PerfReport {
        schema_version: REPORT_SCHEMA_VERSION,
        dshtui_version: env!("CARGO_PKG_VERSION").to_string(),
        run_metadata: RunMetadata {
            fixture: fixture.to_string(),
            dshtui_version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_ms: now_ms,
            duration_ms: started.elapsed().as_millis() as u64,
            seed_sessions,
        },
        results,
        summary,
    };

    let write_ok = write_report(&cfg.report_path, &report);
    let mut exit = exit_code_for(&report.results);
    if !write_ok {
        // 报告是性能门禁的审计产物；写盘失败必须 fail-closed，不能以
        // stdout 摘要冒充可复核的 PASS 报告（REQ-008 错误模型）。
        exit = 1;
    }
    (report, exit)
}

/// under-scale 判定与退出码映射：全 skip → 2；有 fail → 1；否则 0。
fn exit_code_for(results: &[MetricResult]) -> i32 {
    if !results.is_empty() && results.iter().all(|r| r.status == "skip") {
        2
    } else if results.iter().any(|r| r.status == "fail") {
        1
    } else {
        0
    }
}

fn summarize(results: &[MetricResult]) -> String {
    let fails = results.iter().filter(|r| r.status == "fail").count();
    let skips = results.iter().filter(|r| r.status == "skip").count();
    let passed = results.len() - fails - skips;
    if fails == 0 && skips == 0 {
        "全 PASS".to_string()
    } else if fails == 0 {
        format!("{passed} PASS / {skips} skip")
    } else {
        format!("{passed} PASS / {fails} FAIL / {skips} skip")
    }
}

/// 原子写报告：同目录 `<name>.report.tmp` + rename；路径为空 = 不写只返回。
/// 序列化/写盘失败返回 false，由调用方 fail-closed。
fn write_report(path: &Path, report: &PerfReport) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            eprintln!("警告: 报告目录创建失败（路径 {}）", path.display());
            return false;
        }
    }
    let json = match serde_json::to_string_pretty(report) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("警告: PerfReport 序列化失败: {e}");
            return false;
        }
    };
    let tmp = path.with_extension("report.tmp");
    let ok = std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, path).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("警告: 报告写入失败（路径 {}）", path.display());
    }
    ok
}

/// stdout 摘要（main 在 --report 省略时也调用；每行 metric/measured/pass）。
pub fn format_summary(report: &PerfReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "perf 报告: schema=v{} dshtui={} fixture={} duration_ms={} seed_sessions={}\n",
        report.schema_version,
        report.dshtui_version,
        report.run_metadata.fixture,
        report.run_metadata.duration_ms,
        report.run_metadata.seed_sessions
    ));
    for r in &report.results {
        let measured = match r.measured {
            Some(v) if r.unit == "MB" => format!("{v:.2} MB"),
            Some(v) => format!("{v:.2} ms"),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "  {:<22} {:>10}  {:<4}  (阈值 {} {})\n",
            r.metric,
            measured,
            r.status.to_uppercase(),
            r.threshold,
            r.unit
        ));
    }
    out.push_str(&format!(
        "summary: {}  exit={}\n",
        report.summary,
        exit_code_for(&report.results)
    ));
    out
}

/// live 探测（只读）：本机 3080 可达即可（仅 TCP connect，不发业务请求）。
/// 一次失败后短退避重试一次；两连败视为不可达。
fn probe_live_available() -> bool {
    let addr: std::net::SocketAddr = "127.0.0.1:3080".parse().expect("static addr");
    for attempt in 0..2 {
        match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(_) => return true,
            Err(_) if attempt == 0 => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return false,
        }
    }
    false
}

// ============================================================================
// 场景测量（全部进程内 + headless TestBackend；seed 确定性）
// ============================================================================

/// 场景过滤：None=全量；Some(metric)=是否命中该场景。
fn wants(filter: Option<&str>, metric: &str) -> bool {
    filter.is_none() || filter == Some(metric)
}

/// 测量全部场景（fixture gate 之外的场景选择；filter 单场景/全量）。
fn measure_scenarios(filter: Option<&str>, scale: &Scale) -> Vec<Scenario> {
    let mut out = Vec::new();

    if wants(filter, "startup_ms") {
        // startup：AppState::new + 一帧 render（TestBackend）到首屏。
        let t = measure(|| {
            let app = AppState::new(scale.window_cap);
            std::hint::black_box(&app);
            let mut term = test_terminal(80, 24);
            term.draw(|f| crate::ui::render(f, &app))
                .expect("startup draw");
        });
        out.push(Scenario {
            name: "startup_ms",
            measured: Some(t),
        });
    }

    if wants(filter, "first_screen_ms") {
        // first_screen：seed 会话进侧栏 + 首帧 render（120×30）。
        let t = measure(|| {
            let mut app = AppState::new(scale.window_cap);
            seed_sessions(&mut app, scale.seed_sessions, scale.window_cap);
            let mut term = test_terminal(120, 30);
            term.draw(|f| crate::ui::render(f, &app))
                .expect("first-screen draw");
        });
        out.push(Scenario {
            name: "first_screen_ms",
            measured: Some(t),
        });
    }

    if wants(filter, "search_ms") {
        // picker/搜索：CatalogIndex 本地 nucleo（wire ModelCatalog → rebuild）
        // 千级目录多次 query 计时（本地 nucleo，无网络）。
        let idx = seed_catalog_index(scale.catalog_items);
        let t = measure(|| {
            for q in ["v4", "pro", "mini", "model", "prov-3", "deep"] {
                let hits = idx.query(q);
                std::hint::black_box(hits.len());
            }
            std::hint::black_box(&idx);
        });
        out.push(Scenario {
            name: "search_ms",
            measured: Some(t),
        });
    }

    if wants(filter, "scroll_frame_p99_ms") {
        // 滚动帧 p99：27turn/1144step 长会话窗口逐帧滚动 render（真实块、
        // viewport.offset 递增），200 次采样 p99。
        let p99 = measure_scroll_p99(scale);
        out.push(Scenario {
            name: "scroll_frame_p99_ms",
            measured: Some(p99),
        });
    }

    // RSS 三档：空闲 / 流式（长会话窗口驻留）/ 图片密集（缓存预算填满）。
    // 进程内诚实测量：状态驻留时读稳定值（malloc_trim 归还 free-list + 双读
    // 取小）；读完 drop + trim，避免污染后续场景。
    if wants(filter, "idle_rss_mb") {
        let mb = measure_rss(scale.window_cap, |_| {});
        out.push(Scenario {
            name: "idle_rss_mb",
            measured: Some(mb),
        });
    }
    if wants(filter, "stream_rss_mb") {
        let mb = measure_rss(scale.window_cap, |app| {
            seed_long_session(app, scale);
        });
        out.push(Scenario {
            name: "stream_rss_mb",
            measured: Some(mb),
        });
    }
    if wants(filter, "image_rss_mb") {
        let mb = measure_rss(scale.window_cap, |app| {
            seed_image_cache(app);
        });
        out.push(Scenario {
            name: "image_rss_mb",
            measured: Some(mb),
        });
    }
    out
}

/// 计时器（ms）。
fn measure<F: FnOnce()>(f: F) -> f64 {
    let t0 = Instant::now();
    f();
    t0.elapsed().as_secs_f64() * 1000.0
}

/// headless 终端（TestBackend，无 crossterm/网络）。
fn test_terminal(w: u16, h: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(w, h)).expect("TestBackend terminal")
}

/// 稳态 RSS 测量：构造 AppState（cap），跑 prepare（状态驻留），malloc_trim +
/// 双稳定读取小；然后 drop + trim（不污染后续场景）。
fn measure_rss<F: FnOnce(&mut AppState)>(cap: usize, prepare: F) -> f64 {
    // 预热：首次 alloc 的 glibc arena 元数据不计入被测增量。
    let mut app = AppState::new(cap);
    prepare(&mut app);
    std::hint::black_box(&app);
    crate::perf::malloc_trim();
    let a = crate::perf::rss_mb();
    std::thread::sleep(Duration::from_millis(10));
    let b = crate::perf::rss_mb();
    let stable = a.min(b);
    drop(app);
    crate::perf::malloc_trim();
    stable
}

/// 滚动帧 p99 测量（确定性长会话；块索引 offset 递增滚动）。
fn measure_scroll_p99(scale: &Scale) -> f64 {
    let mut app = AppState::new(scale.window_cap);
    seed_long_session(&mut app, scale);
    let block_len = app
        .active_window()
        .map(|w| w.len())
        .filter(|&n| n > 0)
        .unwrap_or(1);

    let mut term = test_terminal(120, 30);
    // 预热一帧（布局/编译路径就绪后再采样）。
    term.draw(|f| crate::ui::render(f, &app))
        .expect("warmup draw");
    let mut samples: Vec<f64> = Vec::with_capacity(scale.scroll_frames);
    for _ in 0..scale.scroll_frames {
        let t0 = Instant::now();
        term.draw(|f| crate::ui::render(f, &app))
            .expect("scroll draw");
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        app.viewport.offset = (app.viewport.offset + 1) % block_len;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite f64"));
    let idx = ((samples.len() as f64) * 0.99).floor() as usize;
    samples[idx.min(samples.len() - 1)]
}

/// 确定性 seed：N 个会话写入侧栏（固定 id/标题枚举；无 RNG、无随机字段）。
/// 顺序确定 + 窗口 touch（SessionStore cap=3，LRU 后仅留最近 3 个窗口）。
fn seed_sessions(app: &mut AppState, n: usize, window_cap: usize) {
    for i in 0..n {
        let id = format!("sess-{:06}", i);
        app.workspaces.upsert_session(SessionMeta {
            id: SessionId(id.clone()),
            title: Some(format!("seed session {i}")),
            cwd: Some("/tmp/proj".into()),
            updated_at_ms: 1_700_000_000_000 + i as i64,
            running: i % 3 == 0,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: None,
            last_turn_preview: Some(format!("preview {i}")),
        });
        // 打开一个会话窗口（触发索引/渲染路径真实工作；LRU 自动淘汰）。
        app.sessions.touch(&id, window_cap);
    }
    app.conn = ConnState::Ready;
    app.focus = Focus::Sidebar;
}

/// seed 一个长会话（27 turn / 1144 step 事件流 → 转录窗口 + 轨迹投影 +
/// 搜索索引），置为活动会话（Center 焦点，滚动浏览态）。走公开 reducer
/// `AppState::handle(AppEvent::FollowSnapshot)`（不经私有字段）。
fn seed_long_session(app: &mut AppState, scale: &Scale) {
    let sid = SessionId("bench-long-session".into());
    app.conn = ConnState::Ready;
    app.active_session = Some(sid.clone());
    app.focus = Focus::Center;
    app.mode = Mode::Normal;
    let records = synth_chat_records(scale.turns, scale.steps);
    let _cmds = app.handle(AppEvent::FollowSnapshot {
        session_id: sid,
        cursor: None,
        records,
        has_more: false,
        projections: None,
    });
    // handle 的 window_changed 已用真实块重建 search_index（active_session
    // 预置）；这里收尾滚动态：浏览而非跟随尾、从窗口头开始逐帧滚。
    app.viewport.follow_tail = false;
    app.viewport.offset = 0;
    app.viewport.height = 28;
}

/// 合成聊天会话事件流：turn/start → 每 step（user/message、
/// assistant/message + text-chunks、tool/call、tool/result）→ turn/end。
/// steps 均摊到各 turn（确定性，无时钟/随机字段）；返回记录数 ≈
/// 5×steps + 2×turns。
fn synth_chat_records(turns: u64, steps: usize) -> Vec<SessionHistoryRecord> {
    let turns_u = turns.max(1);
    let per_turn = steps / turns_u as usize;
    let extra = steps % turns_u as usize;
    let mut seq = 1u64;
    let mut out = Vec::with_capacity(steps * 5 + turns_u as usize * 2);
    for turn in 1..=turns_u {
        let n_steps = per_turn + usize::from((turn as usize - 1) < extra);
        push_event(
            &mut out,
            &mut seq,
            "turn/start",
            serde_json::json!({"turn": turn}),
        );
        for step in 1..=n_steps {
            let user_content = format!(
                "user {turn}-{step}: check the dshtui service log tail and fix the failure, keep the error code {seq} visible"
            );
            push_event(
                &mut out,
                &mut seq,
                "user/message",
                serde_json::json!({"content": user_content}),
            );
            let mid = format!("m-{seq}");
            push_event(
                &mut out,
                &mut seq,
                "assistant/message",
                serde_json::json!({"id": mid}),
            );
            let chunk_text = format!(
                "assistant {turn}-{step}: root cause located at line {seq}, fix applied and verified.\n```bash\necho fixed-{seq}\n```"
            );
            out.push(SessionHistoryRecord::Chunks {
                event: ChunkRow::TextChunks(ChunkData {
                    texts: vec![chunk_text],
                    turn: Some(turn),
                    ..Default::default()
                }),
            });
            let command = format!("dshtui check {seq}");
            push_event(
                &mut out,
                &mut seq,
                "tool/call",
                serde_json::json!({"callId": format!("c{turn}-{step}"), "name": "bash", "arguments": {"command": command}}),
            );
            let result = format!("ok seq={seq} exit=0");
            push_event(
                &mut out,
                &mut seq,
                "tool/result",
                serde_json::json!({"callId": format!("c{turn}-{step}"), "content": result, "isError": false}),
            );
        }
        push_event(
            &mut out,
            &mut seq,
            "turn/end",
            serde_json::json!({"turn": turn}),
        );
    }
    out
}

/// 追加一条带递增 seq 的 event 记录。
fn push_event(
    out: &mut Vec<SessionHistoryRecord>,
    seq: &mut u64,
    event_type: &str,
    data: serde_json::Value,
) {
    out.push(SessionHistoryRecord::Event {
        event: SessionWireEvent {
            event_type: event_type.to_string(),
            seq: Some(SessionSeq(*seq)),
            time: Some(1_700_000_000_000 + *seq as i64),
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(data),
        },
    });
    *seq += 1;
}

/// 合成模型目录 wire 结构 → CatalogIndex（走公开 `rebuild(&ModelCatalog)`，
/// 不经私有 items）。确定性 provider/模型 id。
fn seed_catalog_index(n: usize) -> CatalogIndex {
    let mut groups: Vec<crate::api::types::ModelProviderGroup> = Vec::new();
    let per_provider = n.max(1) / 5 + 1;
    for p in 0..5 {
        let models = (0..per_provider)
            .map(|m| crate::api::types::ModelCatalogModel {
                id: format!("model-{p}-{m:03}"),
                name: format!("Provider {p} Model {m:03} deploy"),
                description: None,
                reasoning: None,
            })
            .collect();
        groups.push(crate::api::types::ModelProviderGroup {
            id: format!("prov-{p}"),
            name: format!("Provider {p}"),
            models,
        });
    }
    let catalog = crate::api::types::ModelCatalog {
        default: None,
        routable_providers: Vec::new(),
        groups,
        failures: Vec::new(),
    };
    let mut idx = CatalogIndex::new();
    idx.rebuild(&catalog);
    idx
}

/// 图片缓存 seed：把默认预算（32MB）用合成条目填满——模拟「图片密集」的
/// LRU 记账面（不真正解码/下载网络图）。合成条目记账 64KB，LRU 会按预算
/// 驱逐最旧直至预算内（AC-004-04 口径），temp_file 用空路径（不入盘）。
fn seed_image_cache(app: &mut AppState) {
    let cache = std::sync::Arc::clone(&app.image_cache);
    cache.set_budget(IMAGE_BUDGET_BYTES);
    for i in 0..(IMAGE_BUDGET_BYTES / IMAGE_ENTRY_BYTES * 4) {
        let id = AttachmentId(format!("bench-img-{i:05}"));
        if cache.acquire(&id) == crate::cache::image_cache::Acquire::Started {
            let _ = cache.complete(
                &id,
                ImageCacheEntry {
                    attachment_id: id.clone(),
                    media_type: MediaType("image/png".to_string()),
                    bytes: IMAGE_ENTRY_BYTES,
                    width: 800,
                    height: 600,
                    temp_file: PathBuf::new(),
                    last_used: 0,
                },
            );
        }
    }
    std::hint::black_box(&cache);
}

/// 报告中的 seed_sessions 元数据：全量/first_screen = 会话列表 seed 数；
/// 其余单场景按该场景实际 seed 的活动会话数反映（审计口径，非 gate）。
fn measure_seed_sessions(filter: Option<&str>, scale: Scale) -> usize {
    if matches!(filter, None | Some("first_screen_ms")) {
        scale.seed_sessions
    } else if matches!(
        filter,
        Some("startup_ms" | "scroll_frame_p99_ms" | "stream_rss_mb")
    ) {
        // 长会话场景 seed 1 个活动会话（列表本身为空）。
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// debug 下单测规模：结构/路径全覆盖但秒级完成（RSS 数值不断言）。
    fn tiny_scale() -> Scale {
        Scale {
            seed_sessions: 8,
            catalog_items: 50,
            turns: 2,
            steps: 12,
            window_cap: 200,
            scroll_frames: 12,
        }
    }

    fn seed_cfg(report: PathBuf) -> BenchConfig {
        BenchConfig {
            report_path: report,
            fixture: FixtureMode::Seed,
            scenario: None,
        }
    }

    /// JSON roundtrip 结构一致断言：measured 是浮点计时，容忍末位十进制表示
    /// 差异（±1e-6 内即为同一测量）。
    fn assert_same_report(back: &PerfReport, report: &PerfReport) {
        assert_eq!(back.schema_version, report.schema_version);
        assert_eq!(back.dshtui_version, report.dshtui_version);
        assert_eq!(back.summary, report.summary);
        assert_eq!(back.run_metadata, report.run_metadata);
        assert_eq!(back.results.len(), report.results.len());
        for (b, r) in back.results.iter().zip(&report.results) {
            assert_eq!(b.metric, r.metric);
            assert_eq!(b.threshold, r.threshold);
            assert_eq!(b.unit, r.unit);
            assert_eq!(b.status, r.status);
            match (b.measured, r.measured) {
                (Some(bv), Some(rv)) => {
                    assert!((bv - rv).abs() < 1e-6, "measured 容差: {bv} vs {rv}");
                }
                (None, None) => {}
                other => panic!("measured 结构不一致: {other:?}"),
            }
        }
    }

    #[test]
    fn threshold_table_covers_all_ac008_metrics() {
        // completeness：阈值表恒含全部 AC-008 度量键（防场景↔表漂移）。
        let keys: Vec<&str> = THRESHOLDS.iter().map(|t| t.metric).collect();
        for want in [
            "startup_ms",
            "first_screen_ms",
            "search_ms",
            "scroll_frame_p99_ms",
            "idle_rss_mb",
            "stream_rss_mb",
            "image_rss_mb",
        ] {
            assert!(keys.contains(&want), "阈值表缺 {want}");
        }
        assert_eq!(THRESHOLDS.len(), 7);
    }

    #[test]
    fn seed_run_small_scale_no_skip_and_report_roundtrips() {
        // seed 全量（小规模）：7 行全测得（无 skip）、JSON roundtrip 等结构；
        // 不硬断言 RSS/阈值数值（debug RSS 必然超阈值，exit 允许 0/1）。
        let (report, exit) = run_scaled(&seed_cfg(PathBuf::new()), tiny_scale());
        assert_eq!(report.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(report.results.len(), THRESHOLDS.len());
        assert!(
            report.results.iter().all(|r| r.status != "skip"),
            "seed fixture 全部场景应测得: {}",
            report.summary
        );
        assert!(matches!(exit, 0 | 1), "无 skip 时退出码只能 0/1: {exit}");
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("NaN"), "skip 行不用 NaN 占位: {json}");
        let back: PerfReport = serde_json::from_str(&json).unwrap();
        assert_same_report(&back, &report);
        assert!(
            report.summary.contains("PASS"),
            "summary={}",
            report.summary
        );
    }

    #[test]
    fn seed_deterministic_sessions_and_catalog() {
        // 确定性：同参数两次 seed 完全一致（无 RNG）。
        let mut a = AppState::new(50);
        seed_sessions(&mut a, 1042, 50);
        let mut b = AppState::new(50);
        seed_sessions(&mut b, 1042, 50);
        assert_eq!(a.workspaces.session_count(), 1042);
        assert_eq!(b.workspaces.session_count(), 1042);
        let ids = |app: &AppState| -> Vec<String> {
            app.workspaces
                .sessions_sorted()
                .into_iter()
                .map(|m| m.id.0.clone())
                .collect()
        };
        assert_eq!(ids(&a), ids(&b), "会话序确定性");

        // 长会话事件流确定性：两次合成 + apply 后窗口块快照一致。
        let ra = synth_chat_records(27, 1144);
        let rb = synth_chat_records(27, 1144);
        assert_eq!(ra.len(), rb.len());
        let mut wa = crate::model::TranscriptWindow::new(SCROLL_WINDOW_CAP);
        wa.apply(crate::model::Incoming::Snapshot {
            cursor: None,
            records: ra,
            has_more: false,
            projections: None,
        });
        let mut wb = crate::model::TranscriptWindow::new(SCROLL_WINDOW_CAP);
        wb.apply(crate::model::Incoming::Snapshot {
            cursor: None,
            records: rb,
            has_more: false,
            projections: None,
        });
        assert_eq!(wa.len(), wb.len());
        let ba: Vec<crate::model::Block> = wa.block_snapshot();
        let bb: Vec<crate::model::Block> = wb.block_snapshot();
        assert_eq!(ba, bb, "长会话块快照确定性");
        assert!(ba.len() > 100, "长会话窗口块数达标: {}", ba.len());
    }

    #[test]
    fn scroll_scenario_produces_p99_row() {
        // 滚动场景产出 p99 值（真实计时，阈值不断言）。
        let scenarios = measure_scenarios(Some("scroll_frame_p99_ms"), &tiny_scale());
        assert_eq!(scenarios.len(), 1);
        let s = scenarios[0];
        assert_eq!(s.name, "scroll_frame_p99_ms");
        assert!(s.measured.is_some(), "scroll 应测得值");
        assert!(s.measured.unwrap() >= 0.0);
    }

    #[test]
    fn empty_report_path_writes_nothing() {
        // report_path 空 = 不写盘：先清掉可能残留的默认产物（手工/CI 跑过
        // release bench 会生成），空路径运行后断言默认路径仍未出现。
        let default = Path::new(DEFAULT_REPORT_PATH);
        let _ = std::fs::remove_file(default);
        let (report, _exit) = run_scaled(&seed_cfg(PathBuf::new()), tiny_scale());
        assert!(!default.exists(), "空 report_path 不应写默认路径");
        assert_eq!(report.results.len(), 7);
    }

    #[test]
    fn report_is_written_atomically_and_parses() {
        // 原子写：给定真实路径 → 同目录产物存在且可解析回同结构。
        let dir = std::env::temp_dir().join(format!(
            "dshtui-bench-test-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perf.json");
        let (report, _exit) = run_scaled(&seed_cfg(path.clone()), tiny_scale());
        assert!(path.exists(), "报告应已写盘");
        let raw = std::fs::read_to_string(&path).unwrap();
        let back: PerfReport = serde_json::from_str(&raw).unwrap();
        assert_same_report(&back, &report);
        // tmp 文件不残留。
        assert!(!path.with_extension("report.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_write_failure_is_nonzero_and_fail_closed() {
        let root =
            std::env::temp_dir().join(format!("dshtui-bench-write-fail-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let parent_file = root.join("parent-file");
        std::fs::write(&parent_file, b"not a directory").unwrap();
        let report_path = parent_file.join("perf.json");
        let (report, exit) = run_scaled(&seed_cfg(report_path.clone()), tiny_scale());
        assert!(report.summary.contains("PASS"));
        assert_eq!(exit, 1, "报告写失败必须 fail-closed");
        assert!(!report_path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exit_code_mapping_pass_fail_and_all_skip() {
        let row = |status: &str| MetricResult {
            metric: "m".into(),
            threshold: 1.0,
            unit: "ms".into(),
            measured: None,
            pass: false,
            status: status.into(),
        };
        assert_eq!(exit_code_for(&[row("pass"), row("pass")]), 0);
        assert_eq!(exit_code_for(&[row("pass"), row("fail")]), 1);
        assert_eq!(exit_code_for(&[row("fail")]), 1);
        assert_eq!(
            exit_code_for(&[row("skip"), row("skip")]),
            2,
            "全 skip = under-scale"
        );
        // 混合 skip + pass 不是全 skip：0（skip 不判成败）。
        assert_eq!(exit_code_for(&[row("pass"), row("skip")]), 0);
        assert_eq!(exit_code_for(&[]), 0);
    }

    #[test]
    fn fixture_parse_accepts_auto_live_seed_and_rejects_unknown() {
        assert_eq!(FixtureMode::parse("auto").unwrap(), FixtureMode::Auto);
        assert_eq!(FixtureMode::parse("live").unwrap(), FixtureMode::Live);
        assert_eq!(FixtureMode::parse("seed").unwrap(), FixtureMode::Seed);
        assert!(FixtureMode::parse("foo").is_err());
        // 默认 fixture = seed（不依赖 3080）。
        assert_eq!(BenchConfig::default().fixture, FixtureMode::Seed);
    }

    #[test]
    fn summary_line_reports_every_metric() {
        // format_summary 覆盖每行 metric/measured/pass（main 摘要路径）。
        let (report, _exit) = run_scaled(&seed_cfg(PathBuf::new()), tiny_scale());
        let text = format_summary(&report);
        for m in [
            "startup_ms",
            "first_screen_ms",
            "search_ms",
            "scroll_frame_p99_ms",
            "idle_rss_mb",
            "stream_rss_mb",
            "image_rss_mb",
        ] {
            assert!(text.contains(m), "摘要缺 {m}:\n{text}");
        }
        assert!(text.contains("summary:"), "摘要含 summary 行");
    }
}
