//! 性能基准 harness（REQ-008 FR-008-01 / AC-008-01~08；D-56）。
//!
//! 形态（Step A 扩展；在 Step 3 形态 A + 吸收 C/B 之上）：单一 lib 入口
//! `dshtui bench [--report PATH] [--report-md PATH] [--fixture auto|live|seed]
//! [--scenario NAME]`。默认 fixture = **auto**（3080 可达 + DSH_TOKEN 就绪 →
//! 真实 `session/list` 侧栏 verified live；否则确定性 seed 兜底），退出码
//! 0=全 PASS / 1=有 FAIL / 2=under-scale 全 skip。产物双份：固定 schema 的
//! PerfReport JSON（原子写 `target/perf/perf-report.json`）+ 人类可读
//! markdown（原子写 `target/perf/perf-report.md`；`--report-md ""` 跳过）。
//!
//! 场景全部为**进程内**测量（被测进程即 `dshtui bench` 自身；release 构建下
//! RSS 与真实 TUI 进程同构——「RSS 单进程诚实测量」口径，Notes/06 §8），
//! 渲染一律走 `ratatui::backend::TestBackend`（headless，不开终端、不发网络）。
//! live verified（AC-008-01）只在 live/auto + 本机 3080 只读可达 + `DSH_TOKEN`
//! 就绪时发生：读 `session/list` 真实会话元数据（id/标题/数），侧栏首屏/搜索
//! 用真实列表测量，`run_metadata.seed_sessions` = 真实会话数；token 缺失 /
//! 不可达 / list 失败 → seed 兜底（fixture 标 `seed-fallback`，附 metadata note，
//! 不 hard-fail——「seed 兜底」口径）。
//!
//! 阈值表集中在 `THRESHOLDS`（一 AC 指标一行），`completeness` 单测锁死表↔
//! 场景不漂移；seed fixture 确定性（固定 id/标题枚举、无 RNG、无时钟字段）
//! 保证双跑一致。fps 为 Min 方向（越大越好），其余 Max 方向（越小越好）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};

use crate::api::types::{
    meta_from_raw, AttachmentId, ChunkData, ChunkRow, MediaType, SessionHistoryRecord, SessionId,
    SessionMeta, SessionSeq, SessionWireEvent,
};
use crate::api::{session, DshClient};
use crate::app::{AppEvent, AppState, ConnState, Focus, Mode};
use crate::model::{CatalogIndex, ImageCacheEntry};

/// 报告 schema 版本（产物格式固定，D-56）。
pub const REPORT_SCHEMA_VERSION: u32 = 1;
/// 默认报告路径（相对运行目录；原子写同目录 tmp+rename）。
pub const DEFAULT_REPORT_PATH: &str = "target/perf/perf-report.json";
/// 默认 markdown 报告路径（与 JSON 并列的双份产物；`--report-md ""` 跳过）。
pub const DEFAULT_REPORT_MD_PATH: &str = "target/perf/perf-report.md";

/// seed fixture 规模常量（REQ-008 AC-008 口径：1042 会话 / 1000 模型 /
/// 27turn·1144step 长会话 / 200 滚动采样帧）。
const SEED_SESSIONS: usize = 1042;
/// 10k 列表场景（D-58 AC-008-06）：侧栏 10_000 会话（确定性 seed，无 RNG）。
const SCALE_SESSIONS_10K: usize = 10_000;
const CATALOG_ITEMS: usize = 1000;
/// 27 turn / 1144 step 合计（均摊到各 turn，确定性无 RNG）。
const SCROLL_TURNS: u64 = 27;
const SCROLL_STEPS: usize = 1144;
/// 长会话窗口容量：容纳 fixture 尾部足够多的真实块参与逐帧 layout。
const SCROLL_WINDOW_CAP: usize = 1200;
const SCROLL_FRAMES: usize = 200;
/// 10k 侧栏场景的 AppState 窗口容量：10k 会话只进侧栏元数据，窗口 LRU 只留
/// 最近 3 个空转录窗口（不持有 10k 个 open transcript window）。
const TEN_K_WINDOW_CAP: usize = 200;
/// page_flip p99 采样翻页次数（首帧渲染计时样本）。
const PAGE_FLIP_SAMPLES: usize = 60;
/// live `session/list` 分页硬上限（防服务端 nextCursor 异常导致死循环）。
const LIVE_LIST_MAX_PAGES: usize = 500;
/// 图片缓存预算（= config `[perf] cache_bytes` 默认 32MB；合成账本填满）。
const IMAGE_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// 单合成图片条目记账字节（64KB × 512 次尝试，LRU 会按预算驱逐至 ~32MB）。
const IMAGE_ENTRY_BYTES: u64 = 64 * 1024;

/// fixture 模式（--fixture）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureMode {
    /// 默认（D-58）：3080 只读可达 + `DSH_TOKEN` 就绪 → live verified（真实
    /// `session/list` 侧栏）；否则确定性 seed 兜底（fixture 标 seed-fallback）。
    Auto,
    /// live verified（AC-008-01）：需要本机 3080 可达 + `DSH_TOKEN` 已设 +
    /// `session/list` 成功；任一缺失 → seed 兜底（标 seed-fallback，不失败）。
    Live,
    /// 确定性 seed（CI 强制；无网络依赖，可复现）。
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

/// 阈值方向：达标方向（D-58 引入混合方向：fps 越大越好，其余越小越好）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoundKind {
    /// 越大越好：pass = measured ≥ bound。
    Min,
    /// 越小越好：pass = measured ≤ bound。
    #[default]
    Max,
}

/// 阈值表：AC-008 指标一行（Notes/06 §7-8 + charter §3.2 + Notes/09 O01~O05）。
/// `bound` 为达标界值（Min 方向=下界，Max 方向=上界）；`unit` 展示用。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Threshold {
    pub metric: &'static str,
    pub bound: f64,
    pub unit: &'static str,
    pub kind: BoundKind,
}

impl Threshold {
    /// 该行 bound 达标判定（方向感知）。
    pub fn passes(&self, measured: f64) -> bool {
        pass_for(self.kind, measured, self.bound)
    }
}

/// 纯函数方向判定（单测直测）：Min 越大越好 / Max 越小越好。
fn pass_for(kind: BoundKind, measured: f64, bound: f64) -> bool {
    match kind {
        BoundKind::Min => measured >= bound,
        BoundKind::Max => measured <= bound,
    }
}

/// 基准阈值（11 项 AC-008 指标；ms / fps / MB 计）。
pub const THRESHOLDS: &[Threshold] = &[
    // AC-008-03：启动到可交互 <1s、列表首屏 <300ms、picker/搜索 <30ms。
    Threshold {
        metric: "startup_ms",
        bound: 1000.0,
        unit: "ms",
        kind: BoundKind::Max,
    },
    Threshold {
        metric: "first_screen_ms",
        bound: 300.0,
        unit: "ms",
        kind: BoundKind::Max,
    },
    Threshold {
        metric: "search_ms",
        bound: 30.0,
        unit: "ms",
        kind: BoundKind::Max,
    },
    // AC-008-04：滚动帧 p99 <33ms（headless 代理：与 fps 同一次滚动采样）。
    Threshold {
        metric: "scroll_frame_p99_ms",
        bound: 33.0,
        unit: "ms",
        kind: BoundKind::Max,
    },
    // D-58 新指标（方向混合）：滚动流畅度 fps ≥15（Min 方向）。
    Threshold {
        metric: "scroll_fps",
        bound: 15.0,
        unit: "fps",
        kind: BoundKind::Min,
    },
    // D-58：长会话「翻页/切窗口」首帧 p99 <33ms（双窗口互切代理）。
    Threshold {
        metric: "page_flip_ms",
        bound: 33.0,
        unit: "ms",
        kind: BoundKind::Max,
    },
    // D-58 / AC-008-06：1 万会话侧栏搜索 <30ms、首屏 <300ms。
    Threshold {
        metric: "list_10k_search_ms",
        bound: 30.0,
        unit: "ms",
        kind: BoundKind::Max,
    },
    Threshold {
        metric: "list_10k_first_screen_ms",
        bound: 300.0,
        unit: "ms",
        kind: BoundKind::Max,
    },
    // AC-008-02：RSS（空闲 <25MB / 流式 <80MB / 图片密集 <150MB，LRU 受控）。
    Threshold {
        metric: "idle_rss_mb",
        bound: 25.0,
        unit: "MB",
        kind: BoundKind::Max,
    },
    Threshold {
        metric: "stream_rss_mb",
        bound: 80.0,
        unit: "MB",
        kind: BoundKind::Max,
    },
    Threshold {
        metric: "image_rss_mb",
        bound: 150.0,
        unit: "MB",
        kind: BoundKind::Max,
    },
];

/// 单指标结果（报告行；`measured` 缺省 = skip，不判成败）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricResult {
    pub metric: String,
    /// 达标界值（阈值；方向见 `kind`）。
    pub threshold: f64,
    pub unit: String,
    /// 实测值（按 unit 计）；None = 未测（status=skip）。
    #[serde(default)]
    pub measured: Option<f64>,
    /// 仅 `measured` 存在时有意义（方向感知：`kind=Min` 时 measured ≥ threshold）。
    pub pass: bool,
    /// pass | fail | skip（skip = 环境/under-scale，不判成败）。
    pub status: String,
    /// 阈值方向（D-58；缺省 Max 兼容旧报告）。
    #[serde(default)]
    pub kind: BoundKind,
}

/// 报告运行元信息（fixture 规模/时间/版本）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunMetadata {
    pub fixture: String,
    pub dshtui_version: String,
    pub started_at_ms: i64,
    pub duration_ms: u64,
    /// 本次实际 seed 的会话数（first_screen 场景规模；live 时为真实列表数）。
    pub seed_sessions: usize,
    /// seed 兜底原因等元注记（live 成功/seed 显式为 None；D-58）。
    #[serde(default)]
    pub note: Option<String>,
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
    /// markdown 报告路径；空串 = 跳过 md 写（D-60）。
    pub report_md_path: PathBuf,
    pub fixture: FixtureMode,
    /// 单场景过滤（None = 全量）；非 gate。
    pub scenario: Option<String>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            report_path: PathBuf::from(DEFAULT_REPORT_PATH),
            report_md_path: PathBuf::from(DEFAULT_REPORT_MD_PATH),
            // REQ-008 D-58：默认 auto（3080 可达 + DSH_TOKEN → live verified；
            // 否则确定性 seed 兜底），CI 用 `--fixture seed` 强制复现。
            fixture: FixtureMode::Auto,
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
    /// 10k 列表场景的侧栏规模（全量 = SCALE_SESSIONS_10K；单测小规模可降）。
    ten_k_sessions: usize,
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
            ten_k_sessions: SCALE_SESSIONS_10K,
        }
    }
}

/// 运行基准（全量规模），返回报告 + 退出码（0 全 PASS / 1 有 FAIL /
/// 2 under-scale 全 skip）。
pub fn run(cfg: &BenchConfig) -> (PerfReport, i32) {
    run_scaled(cfg, Scale::full())
}

/// fixture 决定结果（`decide_fixture` 输出；测试可注入确定性结果）。
#[derive(Debug)]
enum FixtureDecision {
    /// 显式 seed：确定性 seed 测量。
    Seed,
    /// live verified：真实 `session/list` 会话元数据（侧栏源）。
    Live(Vec<SessionMeta>),
    /// live 不可得（不可达/无 token/list 失败）→ seed 兜底（不失败）。
    Fallback { reason: String },
}

/// 根据 fixture 模式决定数据源：seed 恒 seed；live/auto → live-or-fallback。
fn decide_fixture(mode: FixtureMode) -> FixtureDecision {
    match mode {
        FixtureMode::Seed => FixtureDecision::Seed,
        FixtureMode::Live | FixtureMode::Auto => {
            if !probe_live_available() {
                return FixtureDecision::Fallback {
                    reason: "本机 3080 不可达（TCP 探测失败）".to_string(),
                };
            }
            // token 只经 env 读（bench 不加载 config；`--token` 语义不适用）。
            let token = match std::env::var("DSH_TOKEN") {
                Ok(t) if !t.trim().is_empty() => t,
                _ => {
                    return FixtureDecision::Fallback {
                        reason: "DSH_TOKEN 未设置".to_string(),
                    }
                }
            };
            match load_live_sessions(&token) {
                Ok(metas) if !metas.is_empty() => FixtureDecision::Live(metas),
                Ok(_) => FixtureDecision::Fallback {
                    reason: "session/list 返回空列表".to_string(),
                },
                Err(e) => FixtureDecision::Fallback {
                    reason: format!("session/list 加载失败: {e}"),
                },
            }
        }
    }
}

/// run 的可测内核：fixture gate 后测量 → 阈值比对 → 报告 + 退出码。
fn run_scaled(cfg: &BenchConfig, scale: Scale) -> (PerfReport, i32) {
    let decided = decide_fixture(cfg.fixture);
    run_scaled_with(cfg, scale, decided)
}

/// run_scaled 的注入内核（fixture 决定可由测试指定，保持确定性）。
fn run_scaled_with(cfg: &BenchConfig, scale: Scale, decided: FixtureDecision) -> (PerfReport, i32) {
    let started = Instant::now();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // 先消费 fixture 决定：标签/note + live 会话元数据（owned）。live 列表
    // 只喂 startup/first_screen/search 三个列表场景，RSS 三档测独立驻留状态
    // ——列表在进入 RSS 测量前被释放（见 measure_scenarios 内 drop）。
    let (fixture_label, note, live_owned) = match decided {
        FixtureDecision::Seed => ("seed", None, None),
        FixtureDecision::Live(metas) => ("live", None, Some(metas)),
        FixtureDecision::Fallback { reason } => (
            "seed-fallback",
            Some(format!("seed-fallback: {reason}")),
            None,
        ),
    };
    let seed_sessions = match &live_owned {
        Some(metas) => metas.len(),
        None => measure_seed_sessions(cfg.scenario.as_deref(), scale),
    };

    // 场景测量：seed/seed-fallback 用确定性 seed 侧栏；live 用真实会话元数据
    // 构建侧栏（列表场景）。live 列表在此调用内于 RSS 三档前释放。
    let scenarios = measure_scenarios(cfg.scenario.as_deref(), &scale, live_owned);

    // 阈值 → 报告行（每 AC 指标一行；方向感知判定；场景测得 None → skip）。
    let mut results: Vec<MetricResult> = Vec::with_capacity(THRESHOLDS.len());
    for t in THRESHOLDS {
        let measured = scenarios
            .iter()
            .find(|s| s.name == t.metric)
            .and_then(|s| s.measured);
        match measured {
            Some(v) => {
                let pass = t.passes(v);
                results.push(MetricResult {
                    metric: t.metric.to_string(),
                    threshold: t.bound,
                    unit: t.unit.to_string(),
                    measured: Some(v),
                    pass,
                    status: if pass { "pass" } else { "fail" }.to_string(),
                    kind: t.kind,
                });
            }
            None => results.push(MetricResult {
                metric: t.metric.to_string(),
                threshold: t.bound,
                unit: t.unit.to_string(),
                measured: None,
                pass: false,
                status: "skip".to_string(),
                kind: t.kind,
            }),
        }
    }

    let summary = summarize(&results);

    let report = PerfReport {
        schema_version: REPORT_SCHEMA_VERSION,
        dshtui_version: env!("CARGO_PKG_VERSION").to_string(),
        run_metadata: RunMetadata {
            fixture: fixture_label.to_string(),
            dshtui_version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_ms: now_ms,
            duration_ms: started.elapsed().as_millis() as u64,
            seed_sessions,
            note,
        },
        results,
        summary,
    };

    // 双份产物（JSON + markdown）原子写；任一失败 fail-closed——性能门禁的
    // 审计产物缺失时不以 stdout 摘要冒充可复核 PASS（REQ-008 错误模型）。
    let write_ok = write_report(&cfg.report_path, &report);
    let md = render_report_md(&report);
    let write_md_ok = write_report_md(&cfg.report_md_path, &md);
    let mut exit = exit_code_for(&report.results);
    if !write_ok || !write_md_ok {
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

/// 原子写 JSON 报告：同目录 `<name>.report.tmp` + rename；路径为空 = 不写
/// 只返回（成功）。序列化/写盘失败返回 false，由调用方 fail-closed。
fn write_report(path: &Path, report: &PerfReport) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }
    let json = match serde_json::to_string_pretty(report) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("警告: PerfReport 序列化失败: {e}");
            return false;
        }
    };
    atomic_write_text(path, &json)
}

/// 原子写 markdown 报告（D-60；同目录 tmp+rename）。失败同样 fail-closed
/// （JSON/md 双份产物任一缺失都不可复核 → 调用方 exit 1）。
fn write_report_md(path: &Path, md: &str) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }
    atomic_write_text(path, md)
}

/// 原子文本写：建父目录 → `<name>.report.tmp` → rename；失败清理 tmp。
fn atomic_write_text(path: &Path, content: &str) -> bool {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            eprintln!("警告: 报告目录创建失败（路径 {}）", path.display());
            return false;
        }
    }
    let tmp = path.with_extension("report.tmp");
    let ok = std::fs::write(&tmp, content).is_ok() && std::fs::rename(&tmp, path).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("警告: 报告写入失败（路径 {}）", path.display());
    }
    ok
}

/// stdout 摘要（main 在 --report 省略时也调用；每行 metric/measured/pass）。
pub fn format_summary(report: &PerfReport) -> String {
    let mut out = String::new();
    let note = report
        .run_metadata
        .note
        .as_deref()
        .map(|n| format!(" note=\"{n}\""))
        .unwrap_or_default();
    out.push_str(&format!(
        "perf 报告: schema=v{} dshtui={} fixture={} duration_ms={} seed_sessions={}{}\n",
        report.schema_version,
        report.dshtui_version,
        report.run_metadata.fixture,
        report.run_metadata.duration_ms,
        report.run_metadata.seed_sessions,
        note
    ));
    for r in &report.results {
        let measured = match r.measured {
            Some(v) => format!("{v:.2} {}", r.unit),
            None => "-".to_string(),
        };
        let bound = match r.kind {
            BoundKind::Min => format!("≥ {}", fmt_num(r.threshold)),
            BoundKind::Max => format!("≤ {}", fmt_num(r.threshold)),
        };
        out.push_str(&format!(
            "  {:<26} {:>12}  {:<6}  (阈值 {bound} {})\n",
            r.metric,
            measured,
            r.status.to_uppercase(),
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

/// 阈值显示数字（整数省略小数位，读起来干净）。
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// Markdown 报告渲染（D-60）：metadata 头 + 汇总表（metric | measured | bound
/// | unit | status）+ summary 行。确定性（无时钟/随机字段），供单测直测。
pub fn render_report_md(report: &PerfReport) -> String {
    let mut out = String::new();
    out.push_str("# dshtui 性能基准报告（REQ-008）\n\n");
    out.push_str(&format!("- schema_version: {}\n", report.schema_version));
    out.push_str(&format!("- dshtui: {}\n", report.dshtui_version));
    out.push_str(&format!("- fixture: {}\n", report.run_metadata.fixture));
    out.push_str(&format!(
        "- seed_sessions: {}\n",
        report.run_metadata.seed_sessions
    ));
    out.push_str(&format!(
        "- duration_ms: {}\n",
        report.run_metadata.duration_ms
    ));
    if let Some(note) = &report.run_metadata.note {
        out.push_str(&format!("- note: {note}\n"));
    }
    out.push('\n');
    out.push_str("| metric | measured | bound | unit | status |\n");
    out.push_str("|---|---|---|---|---|\n");
    for r in &report.results {
        let measured = match r.measured {
            Some(v) => format!("{v:.2}"),
            None => "-".to_string(),
        };
        let bound = match r.kind {
            BoundKind::Min => format!("≥ {}", fmt_num(r.threshold)),
            BoundKind::Max => format!("≤ {}", fmt_num(r.threshold)),
        };
        out.push_str(&format!(
            "| {} | {measured} | {bound} | {} | {} |\n",
            r.metric, r.unit, r.status
        ));
    }
    out.push('\n');
    out.push_str(&format!("summary: {}\n", report.summary));
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

/// 真实 `session/list` 全量读取（AC-008-01 live verified）→ 侧栏 SessionMeta
/// 列表（meta_from_raw 逐条转换，按 id 去重）。
///
/// bench::run 是同步入口（主 binary 已在 `#[tokio::main]` runtime 内，不能再
/// 嵌套 block_on），因此 live 加载在独立线程里自建 current_thread runtime 跑
/// 异步 connect+list，主线程 30s 内收结果。任何失败返回 Err（调用方 seed 兜底）。
fn load_live_sessions(token: &str) -> Result<Vec<SessionMeta>, String> {
    let token = token.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("live runtime 构建失败: {e}"))
            .and_then(|rt| rt.block_on(load_live_sessions_async(&token)));
        let _ = tx.send(out);
    });
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(|e| format!("live 加载失败/超时: {e}"))?
}

/// live 加载的异步内核（DshClient::connect + `session::list` 分页全量）。
async fn load_live_sessions_async(token: &str) -> Result<Vec<SessionMeta>, String> {
    const LIVE_BASE: &str = "http://127.0.0.1:3080";
    let client = DshClient::connect(LIVE_BASE, token)
        .await
        .map_err(|e| format!("DshClient::connect 认证失败: {e}"))?;
    let mut metas: Vec<SessionMeta> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..LIVE_LIST_MAX_PAGES {
        let page = session::list(&client.http, &client.base, cursor.as_deref())
            .await
            .map_err(|e| format!("session/list 失败: {e}"))?;
        for raw in page.raw_items {
            if let Some(m) = meta_from_raw(raw) {
                if seen.insert(m.id.get().to_string()) {
                    metas.push(m);
                }
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(metas)
}

// ============================================================================
// 场景测量（全部进程内 + headless TestBackend；seed 确定性）
// ============================================================================

/// 场景过滤：None=全量；Some(metric)=是否命中该场景。
fn wants(filter: Option<&str>, metric: &str) -> bool {
    filter.is_none()
        || filter == Some(metric)
        || (filter == Some("list_10k") && metric.starts_with("list_10k_"))
}

/// 测量全部场景（fixture gate 之外的场景选择；filter 单场景/全量）。
/// `live` = 真实会话元数据（仅 live verified 提供，owned；None = 确定性
/// seed 侧栏）。live 列表只喂 startup/first_screen/search 三个列表场景，
/// 在进入 RSS 三档前显式 drop + 该场景自带的 malloc_trim 归还——RSS 测的是
/// 各档独立驻留状态（阈值口径与 seed 一致，不含 live 列表进程基线）。
fn measure_scenarios(
    filter: Option<&str>,
    scale: &Scale,
    mut live: Option<Vec<SessionMeta>>,
) -> Vec<Scenario> {
    let mut out = Vec::new();

    if wants(filter, "startup_ms") {
        // startup：AppState::new + 侧栏就绪 + 一帧 render（TestBackend）到首屏。
        // live：真实列表进侧栏后才算「启动完成」（AC-008-01 server-backed）。
        let live_borrow = live.as_deref();
        let t = measure(|| {
            let mut app = AppState::new(scale.window_cap);
            if let Some(metas) = live_borrow {
                seed_sidebar_metas(&mut app, metas);
            }
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
        // first_screen：seed/live 会话进侧栏 + 首帧 render（120×30）。
        let live_borrow = live.as_deref();
        let t = measure(|| {
            let mut app = AppState::new(scale.window_cap);
            match live_borrow {
                Some(metas) => seed_sidebar_metas(&mut app, metas),
                None => seed_sessions(&mut app, scale.seed_sessions, scale.window_cap),
            }
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
        if let Some(metas) = live.as_deref() {
            // live：对真实会话侧栏做 nucleo 会话搜索（picker 同路径），侧栏
            // 在计时外构建一次，测得的是纯单次查询延迟。
            let mut app = AppState::new(scale.window_cap);
            seed_sidebar_metas(&mut app, metas);
            let queries = live_search_queries(metas);
            let t = measure(|| {
                for q in &queries {
                    let hits = app.workspaces.match_sessions(q);
                    std::hint::black_box(hits.len());
                }
                std::hint::black_box(&app.workspaces);
            });
            out.push(Scenario {
                name: "search_ms",
                measured: Some(t),
            });
        } else {
            // seed：picker/搜索——CatalogIndex 本地 nucleo（wire ModelCatalog
            // → rebuild）千级目录多次 query 计时（本地 nucleo，无网络）。
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
    }

    if wants(filter, "scroll_frame_p99_ms") || wants(filter, "scroll_fps") {
        // 滚动帧 p99 + fps 同一次滚动采样（同一帧循环一次跑出两个指标：
        // p99 = 帧延迟分位；fps = 1000/平均帧 ms，headless 代理，D-58）。
        let samples = measure_scroll_samples(scale);
        if wants(filter, "scroll_frame_p99_ms") {
            out.push(Scenario {
                name: "scroll_frame_p99_ms",
                measured: Some(samples.p99_ms),
            });
        }
        if wants(filter, "scroll_fps") {
            out.push(Scenario {
                name: "scroll_fps",
                measured: Some(samples.fps()),
            });
        }
    }

    if wants(filter, "page_flip_ms") {
        // 翻页/切窗口：双长会话窗口互切（复用两窗口都打开的 seed），渲染
        // 目标首帧计时，N 次采样 p99（<33ms，D-58）。
        let p99 = measure_page_flip_p99(scale);
        out.push(Scenario {
            name: "page_flip_ms",
            measured: Some(p99),
        });
    }

    // 10k 列表场景（D-58 / AC-008-06）：10k 侧栏一次性构建，搜索 + 首屏两个
    // 指标共享；仅在 full 规模跑（单测小规模经 scale.ten_k_sessions 降档）。
    if scale.ten_k_sessions > 0
        && (wants(filter, "list_10k_search_ms") || wants(filter, "list_10k_first_screen_ms"))
    {
        let (search_ms, first_ms) = measure_10k_pair(scale.ten_k_sessions);
        if wants(filter, "list_10k_search_ms") {
            out.push(Scenario {
                name: "list_10k_search_ms",
                measured: Some(search_ms),
            });
        }
        if wants(filter, "list_10k_first_screen_ms") {
            out.push(Scenario {
                name: "list_10k_first_screen_ms",
                measured: Some(first_ms),
            });
        }
    }

    // live 列表已消费完（startup/first_screen/search）；进入 RSS 三档前显式
    // 释放 + trim，让后续测量回到与 seed 一致的进程基线（列表只该喂列表场景）。
    if live.take().is_some() {
        crate::perf::malloc_trim();
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

/// 一次滚动采样循环的两档输出：p99 帧延迟（scroll_frame_p99_ms）+ fps
/// （1000 / 平均帧 ms，scroll_fps；D-58 同一帧循环、一次跑出）。
struct ScrollSamples {
    p99_ms: f64,
    avg_ms: f64,
}

impl ScrollSamples {
    /// fps = 1000 / 平均帧 ms（headless 代理口径）。
    fn fps(&self) -> f64 {
        if self.avg_ms > 0.0 {
            1000.0 / self.avg_ms
        } else {
            0.0
        }
    }
}

/// 滚动采样（确定性长会话；块索引 offset 递增滚动）：返回 p99 + 均值。
fn measure_scroll_samples(scale: &Scale) -> ScrollSamples {
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
    let p99_ms = samples[idx.min(samples.len() - 1)];
    let avg_ms = samples.iter().sum::<f64>() / samples.len() as f64;
    ScrollSamples { p99_ms, avg_ms }
}

/// 翻页/切窗口 p99：两个长会话窗口都已打开（SessionStore LRU cap=3 内），
/// 交替切换 active session 并渲染目标「首帧」，采样 N 次取 p99。
fn measure_page_flip_p99(scale: &Scale) -> f64 {
    let mut app = AppState::new(scale.window_cap);
    // 双窗口打开：两个确定性长会话（同一事件流合成两次，块快照一致）。
    let ids = [
        SessionId::new("bench-flip-a".into()),
        SessionId::new("bench-flip-b".into()),
    ];
    let records = synth_chat_records(scale.turns, scale.steps);
    for sid in &ids {
        open_long_session(&mut app, sid, records.clone());
    }

    let mut term = test_terminal(120, 30);
    // 预热一帧（首个活动会话），布局路径就绪后再采样。
    app.active_session = Some(ids[0].clone());
    reset_viewport_top(&mut app);
    term.draw(|f| crate::ui::render(f, &app))
        .expect("warmup draw");

    let mut samples: Vec<f64> = Vec::with_capacity(PAGE_FLIP_SAMPLES);
    for i in 0..PAGE_FLIP_SAMPLES {
        let target = &ids[i % ids.len()];
        // 切到目标窗口并从窗口头渲染首帧（翻页代理）。
        app.active_session = Some(target.clone());
        reset_viewport_top(&mut app);
        let t0 = Instant::now();
        term.draw(|f| crate::ui::render(f, &app))
            .expect("flip draw");
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite f64"));
    let idx = ((samples.len() as f64) * 0.99).floor() as usize;
    samples[idx.min(samples.len() - 1)]
}

/// 视口置顶（滚动浏览态首帧：从窗口头开始，非 follow 尾）。
fn reset_viewport_top(app: &mut AppState) {
    app.viewport.follow_tail = false;
    app.viewport.offset = 0;
    app.viewport.height = 28;
}

/// 10k 列表场景成对测量：一次构建 10k 会话侧栏（确定性 seed），共享给
/// `list_10k_search_ms`（单次最坏查询延迟）与 `list_10k_first_screen_ms`
/// （首屏一帧）。仅侧栏元数据 + LRU 3 窗口，不持有 10k 个转录窗口。
fn measure_10k_pair(n: usize) -> (f64, f64) {
    let mut app = AppState::new(TEN_K_WINDOW_CAP);
    seed_sessions(&mut app, n, TEN_K_WINDOW_CAP);

    // (a) 搜索：固定探测 query 集（确定性命中不同规模），取最坏单查询
    // 延迟——贴近真实按键的「单次搜索 <30ms」口径（AC-008-06）。
    let mut worst = 0.0f64;
    for q in [
        "sess-009999",
        "session 1234",
        "preview 7777",
        "000042",
        "sess-000001",
    ] {
        let t = measure(|| {
            let hits = app.workspaces.match_sessions(q);
            std::hint::black_box(hits.len());
        });
        worst = worst.max(t);
    }
    let search_ms = worst;

    // (b) 首屏：侧栏 10k 行（rows 全构建 + 排序 + ListItem）一帧 120×30。
    let first_screen_ms = measure(|| {
        let mut term = test_terminal(120, 30);
        term.draw(|f| crate::ui::render(f, &app))
            .expect("10k first-screen draw");
    });
    (search_ms, first_screen_ms)
}

/// 确定性 seed：N 个会话写入侧栏（固定 id/标题枚举；无 RNG、无随机字段）。
/// 顺序确定 + 窗口 touch（SessionStore cap=3，LRU 后仅留最近 3 个窗口）。
fn seed_sessions(app: &mut AppState, n: usize, window_cap: usize) {
    for i in 0..n {
        let id = format!("sess-{:06}", i);
        app.workspaces.upsert_session(SessionMeta {
            id: SessionId::new(id.clone()),
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
    let records = synth_chat_records(scale.turns, scale.steps);
    open_long_session(app, &SessionId::new("bench-long-session".into()), records);
}

/// 打开一个长会话窗口（FollowSnapshot reducer：会话窗口 + 轨迹投影 + 活动
/// 会话 search_index 重建）。`sid` 先置为活动会话（window_changed 语义），
/// 收尾滚动态：浏览而非跟随尾、从窗口头开始。
fn open_long_session(app: &mut AppState, sid: &SessionId, records: Vec<SessionHistoryRecord>) {
    app.conn = ConnState::Ready;
    app.active_session = Some(sid.clone());
    app.focus = Focus::Center;
    app.mode = Mode::Normal;
    let _cmds = app.handle(AppEvent::FollowSnapshot {
        session_id: sid.clone(),
        cursor: None,
        records,
        has_more: false,
        projections: None,
    });
    reset_viewport_top(app);
}

/// live 会话元数据 → 侧栏（`session/list` 同步路径；只 upsert 元数据 + Ready
/// 态，不开转录窗口——首屏渲染/搜索不需要窗口，页面上无 RPC）。
fn seed_sidebar_metas(app: &mut AppState, metas: &[SessionMeta]) {
    for m in metas {
        app.workspaces.upsert_session(m.clone());
    }
    app.conn = ConnState::Ready;
    app.focus = Focus::Sidebar;
}

/// live 搜索 query 集：从真实会话元数据确定性抽取 token（标题首词或 id），
/// 最多 6 条去重——贴近真实按键输入的搜索词。
fn live_search_queries(metas: &[SessionMeta]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in metas {
        let tok = m
            .title
            .as_deref()
            .and_then(|t| t.split_whitespace().next())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .unwrap_or_else(|| m.id.get().to_string());
        if !out.contains(&tok) {
            out.push(tok);
        }
        if out.len() >= 6 {
            break;
        }
    }
    if out.is_empty() {
        out.push("session".to_string());
    }
    out
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
            seq: Some(SessionSeq::new(*seq)),
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
        let id = AttachmentId::new(format!("bench-img-{i:05}"));
        if cache.acquire(&id) == crate::cache::image_cache::Acquire::Started {
            let _ = cache.complete(
                &id,
                ImageCacheEntry {
                    attachment_id: id.clone(),
                    media_type: MediaType::new("image/png".to_string()),
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
/// 10k 单场景 = 10k 侧栏规模；其余单场景按该场景实际 seed 的活动会话数
/// 反映（审计口径，非 gate）。
fn measure_seed_sessions(filter: Option<&str>, scale: Scale) -> usize {
    if matches!(
        filter,
        None | Some(
            "first_screen_ms" | "list_10k" | "list_10k_search_ms" | "list_10k_first_screen_ms"
        )
    ) {
        // 全量含 10k 场景时侧栏最大规模 = 10k；first_screen 单场景 = 1042。
        if matches!(
            filter,
            Some("list_10k" | "list_10k_search_ms" | "list_10k_first_screen_ms")
        ) {
            scale.ten_k_sessions
        } else {
            scale.seed_sessions
        }
    } else if matches!(
        filter,
        Some("startup_ms" | "scroll_frame_p99_ms" | "stream_rss_mb" | "page_flip_ms")
    ) {
        // 长会话场景 seed 活动会话数（page_flip 双窗口 = 2；其余 1）。
        if matches!(filter, Some("page_flip_ms")) {
            2
        } else {
            1
        }
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// debug 下单测规模：结构/路径全覆盖但秒级完成（RSS 数值不断言；
    /// 10k 场景用小 N 走同一代码路径）。
    fn tiny_scale() -> Scale {
        Scale {
            seed_sessions: 8,
            catalog_items: 50,
            turns: 2,
            steps: 12,
            window_cap: 200,
            scroll_frames: 12,
            ten_k_sessions: 8,
        }
    }

    fn seed_cfg(report: PathBuf) -> BenchConfig {
        BenchConfig {
            report_path: report,
            report_md_path: PathBuf::new(),
            fixture: FixtureMode::Seed,
            scenario: None,
        }
    }

    /// cfg + 注入 fixture 决定（fallback/live 测试无需触碰 env/网络）。
    fn cfg_with(
        report: PathBuf,
        fixture: FixtureMode,
        scenario: Option<&str>,
        report_md: PathBuf,
    ) -> BenchConfig {
        BenchConfig {
            report_path: report,
            report_md_path: report_md,
            fixture,
            scenario: scenario.map(|s| s.to_string()),
        }
    }

    /// live 兜底测试用的假会话元数据（确定性）。
    fn fake_meta(id: &str, i: i64) -> SessionMeta {
        SessionMeta {
            id: SessionId::new(id.to_string()),
            title: Some(format!("live session {i}")),
            cwd: Some("/tmp/proj".into()),
            updated_at_ms: 1_700_000_000_000 + i,
            running: false,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: None,
            last_turn_preview: Some(format!("preview {i}")),
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
            assert_eq!(b.kind, r.kind);
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
        // completeness：阈值表恒含全部 11 个 AC-008 度量键（防场景↔表漂移），
        // 且只 scroll_fps 是 Min 方向（越大越好）。
        let keys: Vec<&str> = THRESHOLDS.iter().map(|t| t.metric).collect();
        for want in [
            "startup_ms",
            "first_screen_ms",
            "search_ms",
            "scroll_frame_p99_ms",
            "scroll_fps",
            "page_flip_ms",
            "list_10k_search_ms",
            "list_10k_first_screen_ms",
            "idle_rss_mb",
            "stream_rss_mb",
            "image_rss_mb",
        ] {
            assert!(keys.contains(&want), "阈值表缺 {want}");
        }
        assert_eq!(THRESHOLDS.len(), 11);
        // 方向混合正确性：fps Min（bound 15），其余 Max。
        let fps = THRESHOLDS
            .iter()
            .find(|t| t.metric == "scroll_fps")
            .expect("scroll_fps 行存在");
        assert_eq!(fps.kind, BoundKind::Min);
        assert_eq!(fps.bound, 15.0);
        for t in THRESHOLDS {
            if t.metric != "scroll_fps" {
                assert_eq!(t.kind, BoundKind::Max, "{} 应为 Max 方向", t.metric);
            }
        }
    }

    #[test]
    fn bound_direction_drives_pass_max_and_min() {
        // 纯函数方向判定：Max 行超过上界 → fail；Min（fps）行低于下界 → fail、
        // 达到/超过 → pass。
        let max_row = Threshold {
            metric: "sample_max",
            bound: 30.0,
            unit: "ms",
            kind: BoundKind::Max,
        };
        assert!(!max_row.passes(31.0), "Max 超过上界应 fail");
        assert!(max_row.passes(30.0), "Max 等于上界应 pass");
        assert!(max_row.passes(29.0), "Max 低于上界应 pass");
        let min_row = Threshold {
            metric: "sample_fps",
            bound: 15.0,
            unit: "fps",
            kind: BoundKind::Min,
        };
        assert!(!min_row.passes(14.9), "Min 低于下界应 fail");
        assert!(min_row.passes(15.0), "Min 等于下界应 pass");
        assert!(min_row.passes(60.0), "Min 超过下界应 pass");
        // pass_for 单测（独立于 Threshold 构造）。
        assert!(pass_for(BoundKind::Max, 5.0, 10.0));
        assert!(!pass_for(BoundKind::Max, 11.0, 10.0));
        assert!(pass_for(BoundKind::Min, 100.0, 15.0));
        assert!(!pass_for(BoundKind::Min, 10.0, 15.0));
    }

    #[test]
    fn seed_run_small_scale_no_skip_and_report_roundtrips() {
        // seed 全量（小规模）：11 行全测得（无 skip）、JSON roundtrip 等结构；
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
                .map(|m| m.id.get())
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
        let scenarios = measure_scenarios(Some("scroll_frame_p99_ms"), &tiny_scale(), None);
        assert_eq!(scenarios.len(), 1);
        let s = scenarios[0];
        assert_eq!(s.name, "scroll_frame_p99_ms");
        assert!(s.measured.is_some(), "scroll 应测得值");
        assert!(s.measured.unwrap() >= 0.0);
    }

    #[test]
    fn scroll_pair_produces_p99_and_fps_from_one_run() {
        // D-58：scroll_frame_p99_ms 与 scroll_fps 同一次滚动采样两行产出。
        let scenarios = measure_scenarios(None, &tiny_scale(), None);
        let p99 = scenarios
            .iter()
            .find(|s| s.name == "scroll_frame_p99_ms")
            .expect("p99 行存在");
        let fps = scenarios
            .iter()
            .find(|s| s.name == "scroll_fps")
            .expect("fps 行存在");
        assert!(p99.measured.is_some() && fps.measured.is_some());
        assert!(fps.measured.unwrap() > 0.0, "fps 应为正数");
    }

    #[test]
    fn page_flip_scenario_produces_row() {
        // 双窗口翻页：单场景测得 p99（真实计时，阈值不断言）。
        let scenarios = measure_scenarios(Some("page_flip_ms"), &tiny_scale(), None);
        assert_eq!(scenarios.len(), 1);
        let s = scenarios[0];
        assert_eq!(s.name, "page_flip_ms");
        assert!(s.measured.is_some(), "page_flip 应测得值");
        assert!(s.measured.unwrap() >= 0.0);
    }

    #[test]
    fn list_10k_scenario_alias_produces_both_metrics() {
        let scenarios = measure_scenarios(Some("list_10k"), &tiny_scale(), None);
        let names: Vec<&str> = scenarios.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec!["list_10k_search_ms", "list_10k_first_screen_ms"]
        );
        assert!(scenarios.iter().all(|s| s.measured.is_some()));
        assert_eq!(measure_seed_sessions(Some("list_10k"), tiny_scale()), 8);
    }

    #[test]
    fn ten_k_scale_constant_wiring_and_sidebar_determinism() {
        // 10k 常量接线存在 + 10k 构建确定性（两次 seed 顺序一致）；全量 10k
        // 双跑放 release 门禁，单测用小 N 覆盖同一确定性路径。
        assert_eq!(SCALE_SESSIONS_10K, 10_000);
        assert_eq!(Scale::full().ten_k_sessions, SCALE_SESSIONS_10K);
        let n = 4096;
        let mut a = AppState::new(TEN_K_WINDOW_CAP);
        seed_sessions(&mut a, n, TEN_K_WINDOW_CAP);
        let mut b = AppState::new(TEN_K_WINDOW_CAP);
        seed_sessions(&mut b, n, TEN_K_WINDOW_CAP);
        let ids = |app: &AppState| -> Vec<String> {
            app.workspaces
                .sessions_sorted()
                .into_iter()
                .map(|m| m.id.get())
                .collect()
        };
        assert_eq!(a.workspaces.session_count(), n);
        assert_eq!(ids(&a), ids(&b), "10k 侧栏确定性（id 顺序）");
        assert_eq!(ids(&a).len(), n);
        // 10k 场景成对产出（小 N 同一代码路径）。
        let (search_ms, first_ms) = measure_10k_pair(32);
        assert!(search_ms >= 0.0 && first_ms >= 0.0);
    }

    #[test]
    fn auto_live_unavailable_falls_back_to_seed_and_measures() {
        // AC-008-01 seed 兜底：auto + live 不可得（注入 Fallback 决定，不经
        // env/网络）→ fixture=seed-fallback、note 带原因、行不全 skip
        // （seed 兜底真实测量）。
        let cfg = cfg_with(PathBuf::new(), FixtureMode::Auto, None, PathBuf::new());
        let (report, exit) = run_scaled_with(
            &cfg,
            tiny_scale(),
            FixtureDecision::Fallback {
                reason: "测试注入：3080 不可达".to_string(),
            },
        );
        assert_eq!(report.run_metadata.fixture, "seed-fallback");
        assert_eq!(
            report.run_metadata.note.as_deref(),
            Some("seed-fallback: 测试注入：3080 不可达")
        );
        assert!(
            report.results.iter().all(|r| r.status != "skip"),
            "seed 兜底应真实测量: {}",
            report.summary
        );
        // debug 下 RSS 行可能 fail → 退出码允许 0/1，但绝不是 2（全 skip）。
        assert!(
            matches!(exit, 0 | 1),
            "seed 兜底不应 under-scale exit 2: {exit}"
        );
        // live 决定（注入成功）→ fixture=live、seed_sessions=真实数。
        let metas = vec![fake_meta("sess-live-1", 1), fake_meta("sess-live-2", 2)];
        let cfg_live = cfg_with(
            PathBuf::new(),
            FixtureMode::Live,
            Some("first_screen_ms"),
            PathBuf::new(),
        );
        let (report_live, _) =
            run_scaled_with(&cfg_live, tiny_scale(), FixtureDecision::Live(metas));
        assert_eq!(report_live.run_metadata.fixture, "live");
        assert_eq!(report_live.run_metadata.seed_sessions, 2);
        assert!(report_live.run_metadata.note.is_none());
    }

    #[test]
    fn empty_report_path_writes_nothing() {
        // report_path 空 = 不写盘：先清掉可能残留的默认产物（手工/CI 跑过
        // release bench 会生成），空路径运行后断言默认路径仍未出现。
        let default = Path::new(DEFAULT_REPORT_PATH);
        let _ = std::fs::remove_file(default);
        let (report, _exit) = run_scaled(&seed_cfg(PathBuf::new()), tiny_scale());
        assert!(!default.exists(), "空 report_path 不应写默认路径");
        assert_eq!(report.results.len(), 11);
    }

    #[test]
    fn markdown_report_is_deterministic_and_covers_metrics() {
        // D-60：md 渲染确定性 + 覆盖每行 metric + summary 行 + metadata 头。
        let (report, _exit) = run_scaled(&seed_cfg(PathBuf::new()), tiny_scale());
        let a = render_report_md(&report);
        let b = render_report_md(&report);
        assert_eq!(a, b, "md 渲染必须确定性");
        for t in THRESHOLDS {
            assert!(a.contains(t.metric), "md 缺 {}\n{a}", t.metric);
        }
        assert!(a.contains("summary: "), "md 缺 summary 行:\n{a}");
        assert!(a.contains("| metric | measured | bound | unit | status |"));
        assert!(a.contains("fixture:"), "md 缺 metadata 头:\n{a}");
        assert!(a.contains("seed_sessions:"), "md 缺 seed_sessions 头:\n{a}");
    }

    #[test]
    fn md_and_json_report_written_atomically_together() {
        // 双份产物：同目录 JSON + md 都原子写、可读回、无 tmp 残留；md 内容
        // 与 render_report_md 一致。
        let dir = std::env::temp_dir().join(format!(
            "dshtui-bench-md-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let json_path = dir.join("perf.json");
        let md_path = dir.join("perf.md");
        let mut cfg = seed_cfg(json_path.clone());
        cfg.report_md_path = md_path.clone();
        let (report, exit) = run_scaled(&cfg, tiny_scale());
        // debug 下 RSS 行会 fail（RSS 阈值仅 release 达标）→ exit 允许 0/1；
        // 双写成功的关键断言是产物都在 + 无 tmp 残留 + md 与渲染一致。
        assert!(matches!(exit, 0 | 1), "双写运行退出码异常: {exit}");
        assert!(json_path.exists() && md_path.exists(), "JSON + md 都应写盘");
        let md_raw = std::fs::read_to_string(&md_path).unwrap();
        assert_eq!(md_raw, render_report_md(&report));
        assert!(!json_path.with_extension("report.tmp").exists());
        assert!(!md_path.with_extension("report.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn md_write_failure_is_fail_closed() {
        // md 写失败同样 fail-closed（exit 1；JSON 已写但审计产物不全不可信）。
        let root =
            std::env::temp_dir().join(format!("dshtui-bench-md-fail-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let json_path = root.join("perf.json");
        let parent_file = root.join("parent-file");
        std::fs::write(&parent_file, b"not a directory").unwrap();
        let mut cfg = seed_cfg(json_path.clone());
        cfg.report_md_path = parent_file.join("perf.md");
        let (_report, exit) = run_scaled(&cfg, tiny_scale());
        assert_eq!(exit, 1, "md 写失败必须 fail-closed");
        assert!(json_path.exists(), "JSON 已写（md 失败仍 fail-closed）");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn md_default_path_is_derived_from_json_report() {
        // CLI 语义：未给 --report-md 时默认 md = JSON 路径 .md 派生。
        assert_eq!(
            Path::new("target/perf/perf-report.json").with_extension("md"),
            Path::new(DEFAULT_REPORT_MD_PATH)
        );
        assert_eq!(
            Path::new("x/y/out.json").with_extension("md"),
            Path::new("x/y/out.md")
        );
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
            kind: BoundKind::Max,
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
        // 默认 fixture = auto（D-58：live-if-available else seed 兜底）。
        assert_eq!(BenchConfig::default().fixture, FixtureMode::Auto);
        assert_eq!(
            BenchConfig::default().report_md_path,
            PathBuf::from(DEFAULT_REPORT_MD_PATH)
        );
    }

    #[test]
    fn summary_line_reports_every_metric() {
        // format_summary 覆盖每行 metric/measured/pass（main 摘要路径）。
        let (report, _exit) = run_scaled(&seed_cfg(PathBuf::new()), tiny_scale());
        let text = format_summary(&report);
        for t in THRESHOLDS {
            assert!(text.contains(t.metric), "摘要缺 {}:\n{text}", t.metric);
        }
        assert!(text.contains("summary:"), "摘要含 summary 行");
        assert!(text.contains("fps"), "摘要含 fps 单位行");
    }
}
