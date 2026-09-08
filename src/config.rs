//! Config loading and CLI parsing (REQ-001 §3 input contract / Notes/02 §7).
//!
//! Security constraints (REQ-001 §7, D-2=A):
//! - `--token` is an **environment variable name selector** (default
//!   `DSH_TOKEN`); the secret never enters argv;
//! - token precedence: CLI selector > env var `DSH_TOKEN` > config file
//!   `server.token` > interactive paste;
//! - log/display output must be redacted (`redact`); non-loopback addresses
//!   trigger an explicit warning.

use std::env;
use std::path::PathBuf;

use serde::Deserialize;

/// CLI parse result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cli {
    pub url: Option<String>,
    /// Environment variable name selector (D-2), not the secret itself.
    pub token_env: Option<String>,
    pub log_file: Option<String>,
    pub action: CliAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CliAction {
    #[default]
    Run,
    Help,
    Version,
    /// `dshtui monitor [--addr …]`（REQ-009 V0.3 Agent Town 监控面板）。
    Monitor {
        addr: Option<String>,
    },
}

/// Full structure of `~/.config/dshtui/config.toml` (Notes/02 §7 draft).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub ui: UiConfig,
    pub perf: PerfConfig,
    pub keymap: KeymapConfig,
    pub monitor: MonitorConfig,
    /// Draft persistence (ADR-010): enabled toggle + startup clear.
    pub drafts: DraftsConfig,
    /// Export default target path (ADR-010).
    pub export: ExportConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub theme: String,
    /// 语义角色 → 颜色覆盖（role → hex/名色；非法值回退该角色默认并警告，
    /// AC-007-21 语义）。内层 flatten：未知角色保留可警告、不 deny 崩溃。
    #[serde(default)]
    pub palette: std::collections::BTreeMap<String, String>,
    pub sidebar_width_cells: u16,
    /// 右栏详情列宽（REQ-005 V0.3，Notes/04 §1：45 默认，30–60 可调；
    /// clamp 在 `validate` 执行）。
    pub details_width_cells: u16,
    pub show_turn_rail: bool,
    /// Timeline 缩略条（REQ-007 AC-007-29；默认关）。
    pub show_timeline: bool,
    /// `:edit` 编辑器选择链第三级（D-52：`$VISUAL`→`$EDITOR`→config
    /// `[ui].editor`；None = 前两级缺失时提示用户）。
    pub editor: Option<String>,
    /// 图片本地软上限（REQ-007 D-51）：单条消息附件数量上限（默认 10）。
    #[serde(default = "default_max_image_count")]
    pub max_image_count: usize,
    /// 图片本地软上限（REQ-007 D-51）：单张附件字节上限（默认 20 MiB）。
    #[serde(default = "default_max_image_bytes")]
    pub max_image_bytes: u64,
    pub tick_ms: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PerfConfig {
    pub window_messages: usize,
    pub page_size: usize,
    pub cache_bytes: u64,
    pub rss_target_mb: u64,
}

/// `[keymap]` — per-mode command→key-sequence override table (REQ-007
/// AC-007-21; Step 5). Inner layer is a flatten map: unknown mode/command
/// names are preserved and warned at build, never fatal.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct KeymapConfig {
    /// mode → (command → key sequence). `none` unbinds; double-key sequences
    /// like "g g" supported.
    #[serde(default)]
    pub modes: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

/// `[drafts]` — draft persistence switches (ADR-010; D-42 boundary).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DraftsConfig {
    /// 落盘开关：false 退化纯内存注册表（REQ-003 既有语义）。默认开。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 启动时清空 drafts.toml（不残留旧会话草稿）。
    #[serde(default)]
    pub clear: bool,
}

impl Default for DraftsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            clear: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// 图片本地软上限默认：单条消息附件数量 ≤10（REQ-007 D-51）。
fn default_max_image_count() -> usize {
    10
}

/// 图片本地软上限默认：单张 ≤20 MiB（REQ-007 D-51）。
fn default_max_image_bytes() -> u64 {
    20 * 1024 * 1024
}

/// `[export]` — export default target path (ADR-010).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ExportConfig {
    /// 导出默认落盘目录（空 = 提示用户选择）。
    pub default_dir: String,
}

/// `dshtui monitor` 配置段（REQ-009 §3 输入契约；D-29 直连 OTR agent-server）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    /// agent-server 地址（默认 http://127.0.0.1:8799）。
    pub addr: String,
    /// `/agents` 轮询间隔。
    pub poll_agents_ms: u64,
    /// `/kb-stats` 轮询间隔。
    pub poll_kb_ms: u64,
}

/// Effective merged config (CLI overrides file).
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub url: String,
    pub token: Option<String>,
    pub ui: UiConfig,
    pub perf: PerfConfig,
    pub monitor: MonitorConfig,
    pub drafts: DraftsConfig,
    pub export: ExportConfig,
    /// `[keymap]` 覆盖（REQ-007 AC-007-21）；empty = 内置键位。
    pub keymap: KeymapConfig,
}

pub const DEFAULT_URL: &str = "http://127.0.0.1:3080";
pub const DEFAULT_TOKEN_ENV: &str = "DSH_TOKEN";
pub const DEFAULT_MONITOR_ADDR: &str = "http://127.0.0.1:8799";
const DEFAULT_WINDOW_MESSAGES: usize = 200;
const DEFAULT_PAGE_SIZE: usize = 50;
const DEFAULT_TICK_MS: u64 = 33;
const DEFAULT_SIDEBAR_WIDTH: u16 = 32;
/// Details 列宽默认 45（Notes/04 §1；与 ui/layout DEFAULT_DETAILS_WIDTH 同值）。
const DEFAULT_DETAILS_WIDTH_CELLS: u16 = 45;
/// Details 列宽 clamp 边界（30–60，Notes/04 §1）。
const DETAILS_WIDTH_CELLS_MIN: u16 = 30;
const DETAILS_WIDTH_CELLS_MAX: u16 = 60;
/// 图片缓存预算默认 32MB（REQ-004 §3；pub 供 AppState 默认缓存构造）。
pub const DEFAULT_CACHE_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_RSS_TARGET_MB: u64 = 80;
/// `/agents` 轮询间隔默认 2s（FR-009-02）。
const DEFAULT_POLL_AGENTS_MS: u64 = 2_000;
/// `/kb-stats` 轮询间隔默认 30s（FR-009-02）。
const DEFAULT_POLL_KB_MS: u64 = 30_000;

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            token: String::new(),
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            palette: std::collections::BTreeMap::new(),
            sidebar_width_cells: DEFAULT_SIDEBAR_WIDTH,
            details_width_cells: DEFAULT_DETAILS_WIDTH_CELLS,
            show_turn_rail: false,
            show_timeline: false,
            editor: None,
            max_image_count: default_max_image_count(),
            max_image_bytes: default_max_image_bytes(),
            tick_ms: DEFAULT_TICK_MS,
        }
    }
}

impl Default for PerfConfig {
    fn default() -> Self {
        Self {
            window_messages: DEFAULT_WINDOW_MESSAGES,
            page_size: DEFAULT_PAGE_SIZE,
            cache_bytes: DEFAULT_CACHE_BYTES,
            rss_target_mb: DEFAULT_RSS_TARGET_MB,
        }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            addr: DEFAULT_MONITOR_ADDR.to_string(),
            poll_agents_ms: DEFAULT_POLL_AGENTS_MS,
            poll_kb_ms: DEFAULT_POLL_KB_MS,
        }
    }
}

impl Config {
    /// Load the config file; a missing file returns the default config
    /// (no error).
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path(),
        };
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| ConfigError::Io {
            path: path.clone(),
            source: e,
        })?;
        let cfg: Config = toml::from_str(&raw).map_err(|e| ConfigError::Toml {
            path,
            message: e.to_string(),
        })?;
        cfg.validate()
    }

    /// Merge CLI overrides and resolve the token source (interactive paste is
    /// excluded — the caller fills it in on the TTY).
    pub fn resolve(&self, cli: &Cli) -> Result<Effective, ConfigError> {
        let url = cli.url.clone().unwrap_or_else(|| {
            if self.server.url.trim().is_empty() {
                DEFAULT_URL.to_string()
            } else {
                self.server.url.clone()
            }
        });

        // token precedence: CLI selector > DSH_TOKEN > config file > none
        // (interactive paste).
        let token = match &cli.token_env {
            Some(name) => Some(resolve_token_env(name)?),
            None => match env::var(DEFAULT_TOKEN_ENV) {
                Ok(v) if !v.is_empty() => Some(v),
                _ => {
                    if !self.server.token.is_empty() {
                        Some(self.server.token.clone())
                    } else {
                        None
                    }
                }
            },
        };

        // monitor：CLI `--addr` 覆盖配置文件 [monitor].addr（REQ-009 §3）。
        let mut monitor = self.monitor.clone();
        if let CliAction::Monitor {
            addr: Some(addr), ..
        } = &cli.action
        {
            monitor.addr = addr.clone();
        }

        Ok(Effective {
            url,
            token,
            ui: self.ui.clone(),
            perf: self.perf.clone(),
            monitor,
            drafts: self.drafts.clone(),
            export: self.export.clone(),
            keymap: self.keymap.clone(),
        })
    }

    fn validate(mut self) -> Result<Self, ConfigError> {
        if self.perf.window_messages == 0 {
            return Err(ConfigError::InvalidValue {
                path: PathBuf::new(),
                message: "perf.window_messages 必须 > 0".to_string(),
            });
        }
        if self.perf.page_size == 0 {
            return Err(ConfigError::InvalidValue {
                path: PathBuf::new(),
                message: "perf.page_size 必须 > 0".to_string(),
            });
        }

        // REQ-005 §10：详情列宽 clamp 30–60（越界值收敛而非报错，配置无
        // 破坏性迁移；ui/layout split 亦 clamp 兜底）。
        self.ui.details_width_cells = self
            .ui
            .details_width_cells
            .clamp(DETAILS_WIDTH_CELLS_MIN, DETAILS_WIDTH_CELLS_MAX);

        if self.monitor.poll_agents_ms == 0 || self.monitor.poll_kb_ms == 0 {
            return Err(ConfigError::InvalidValue {
                path: PathBuf::new(),
                message: "monitor.poll_agents_ms / monitor.poll_kb_ms 必须 > 0".to_string(),
            });
        }
        Ok(self)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取配置失败 {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("解析配置失败 {path}: {message}")]
    Toml { path: PathBuf, message: String },
    #[error("配置值非法: {message}")]
    InvalidValue { path: PathBuf, message: String },
    #[error("环境变量 {name} 未设置或为空（--token 是环境变量名选择器）")]
    TokenEnvMissing { name: String },
}

fn resolve_token_env(name: &str) -> Result<String, ConfigError> {
    match env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(ConfigError::TokenEnvMissing {
            name: name.to_string(),
        }),
    }
}

/// Default config path: `$XDG_CONFIG_HOME/dshtui/config.toml` or
/// `~/.config/dshtui/config.toml`.
pub fn default_config_path() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("dshtui").join("config.toml");
        }
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("dshtui")
            .join("config.toml");
    }
    PathBuf::from(".config").join("dshtui").join("config.toml")
}

/// Default state path (ADR-010): `$XDG_STATE_HOME/dshtui/drafts.toml` or
/// `~/.local/state/dshtui/drafts.toml`.
pub fn default_state_path() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("dshtui").join("drafts.toml");
        }
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("dshtui")
            .join("drafts.toml");
    }
    PathBuf::from(".local")
        .join("state")
        .join("dshtui")
        .join("drafts.toml")
}

// ---------- ADR-010 atomic write (0600, temp+rename same filesystem) ----------

/// Atomically write `content` to `path` with mode 0600 (ADR-010).
///
/// The temp file lives in the SAME directory as the target (same filesystem
/// rename; never /tmp across calls — `uncategorized/TASK-002-pitfall`). On any
/// failure the temp file is removed; the destination is never half-written.
pub fn atomic_write_0600(path: &std::path::Path, content: &str) -> Result<(), ConfigError> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dshtui".into()),
        std::process::id()
    ));
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|source| ConfigError::Io {
                    path: tmp.clone(),
                    source,
                })?;
            use std::io::Write;
            f.write_all(content.as_bytes())
                .map_err(|source| ConfigError::Io {
                    path: tmp.clone(),
                    source,
                })?;
        }
        #[cfg(not(unix))]
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp).map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(content.as_bytes())
                .map_err(|source| ConfigError::Io {
                    path: tmp.clone(),
                    source,
                })?;
        }
    }
    // 目标目录内原子 rename（同文件系统）。
    std::fs::rename(&tmp, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Re-write `config.toml` preserving all sections while updating the
/// `[ui] theme/palette` values (ADR-010; runtime theme switch persistence,
/// AC-007-20). `palette_override` entries replace the whole palette map when
/// Some (theme switch keeps it stable); a None leaves palette untouched.
///
/// The rewrite parses the existing file to TOML `Value`, mutates the ui
/// table, and serializes back — any unknown section survives unchanged.
pub fn save_theme_config(
    path: &std::path::Path,
    theme: &str,
    palette: &std::collections::BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    update_toml_table(path, "ui", |ui| {
        ui.insert("theme".into(), toml::Value::String(theme.to_string()));
        let palette_value = toml::Value::Table(
            palette
                .iter()
                .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
                .collect(),
        );
        ui.insert("palette".into(), palette_value);
    })
}

/// Re-write `config.toml` preserving all sections while replacing the
/// `[keymap]` modes table (ADR-010; keymap override persistence, AC-007-21).
pub fn save_keymap_config(
    path: &std::path::Path,
    modes: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
) -> Result<(), ConfigError> {
    update_toml_table(path, "keymap", |keymap| {
        let modes_value = toml::Value::Table(
            modes
                .iter()
                .map(|(mode, binds)| {
                    let table = toml::Value::Table(
                        binds
                            .iter()
                            .map(|(cmd, seq)| (cmd.clone(), toml::Value::String(seq.clone())))
                            .collect(),
                    );
                    (mode.clone(), table)
                })
                .collect(),
        );
        keymap.insert("modes".into(), modes_value);
    })
}

/// Shared config re-writer: loads the file (or default when missing), applies
/// `edit` to one named top-level table, serializes and atomic-writes 0600.
fn update_toml_table(
    path: &std::path::Path,
    table_name: &str,
    edit: impl FnOnce(&mut toml::map::Map<String, toml::Value>),
) -> Result<(), ConfigError> {
    let existing: toml::Value = if path.exists() {
        let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        toml::from_str(&raw).map_err(|e| ConfigError::Toml {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let mut table = match existing {
        toml::Value::Table(t) => t,
        _ => toml::map::Map::new(),
    };
    let mut ui = table
        .get(table_name)
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();
    edit(&mut ui);
    table.insert(table_name.to_string(), toml::Value::Table(ui));
    let out = toml::to_string(&toml::Value::Table(table)).map_err(|e| ConfigError::Toml {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    atomic_write_0600(path, &out)
}

/// Parse CLI args. `--token` accepts only an environment variable name; the
/// secret never enters argv. `dshtui monitor [--addr …] [--log …]` 进入
/// REQ-009 监控面板子命令。
/// Compatible with the full argv form: the first argument not starting with
/// `-` is treated as the program name and skipped (argv[0]).
pub fn parse_cli<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut it = args.into_iter().peekable();
    // Skip argv[0] (program name).
    if let Some(first) = it.peek() {
        if !first.starts_with('-') {
            it.next();
        }
    }
    // `monitor` 位置参数（REQ-009 §3 CLI 输入契约）。
    if it.peek().is_some_and(|a| a == "monitor") {
        it.next();
        let mut addr: Option<String> = None;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--help" | "-h" => cli.action = CliAction::Help,
                "--version" | "-V" => cli.action = CliAction::Version,
                "--addr" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--addr 需要一个地址参数".to_string())?;
                    addr = Some(v);
                }
                "--log" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--log 需要一个文件路径参数".to_string())?;
                    cli.log_file = Some(v);
                }
                other => return Err(format!("未知参数: {other}（--help 查看用法）")),
            }
        }
        if matches!(cli.action, CliAction::Help | CliAction::Version) {
            return Ok(cli);
        }
        cli.action = CliAction::Monitor { addr };
        return Ok(cli);
    }
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => cli.action = CliAction::Help,
            "--version" | "-V" => cli.action = CliAction::Version,
            "--url" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--url 需要一个 URL 参数".to_string())?;
                cli.url = Some(v);
            }
            "--token" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--token 需要一个环境变量名参数".to_string())?;
                if v.contains('=') || looks_like_secret(&v) {
                    return Err(format!(
                        "--token 是环境变量名选择器（如 DSH_TOKEN），不接受 secret 本身：{v}"
                    ));
                }
                cli.token_env = Some(v);
            }
            "--log" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--log 需要一个文件路径参数".to_string())?;
                cli.log_file = Some(v);
            }
            other => return Err(format!("未知参数: {other}（--help 查看用法）")),
        }
    }
    Ok(cli)
}

/// Heuristic: environment variable names that look like secrets (long, with
/// special characters) are rejected when passed via `--token`.
fn looks_like_secret(v: &str) -> bool {
    v.len() > 64
        || !v
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Determine whether a URL is loopback (loopback-only by default; a
/// non-loopback address needs an explicit warning, REQ §7).
pub fn is_loopback(url: &str) -> bool {
    matches!(
        host_of(url).to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Extract the URL host (supports the `[::1]:3080` IPv6 form and user@host).
fn host_of(url: &str) -> &str {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or(inner)
    } else {
        match hostport.find(':') {
            Some(i) => &hostport[..i],
            None => hostport,
        }
    }
}

/// Redact config/env display and logging: the token is never printed.
pub fn redact_summary(eff: &Effective) -> String {
    format!(
        "url={} token={} window_messages={} page_size={} tick_ms={}",
        eff.url,
        if eff.token.is_some() { "***" } else { "<none>" },
        eff.perf.window_messages,
        eff.perf.page_size,
        eff.ui.tick_ms
    )
}

pub fn usage_text() -> String {
    format!(
        "\
dshtui {version} — 官方 dsh web Remote API 的 Rust TUI 客户端

用法: dshtui [--url <url>] [--token <env-name>] [--log <file>] [--help] [--version]
      dshtui monitor [--addr <agent-server>] [--log <file>]

选项:
  --url <url>       dsh web 地址（默认 http://127.0.0.1:3080）
  --token <env>     环境变量名选择器（默认 {token_env}），secret 不出现在 argv
  --log <file>      日志文件路径（默认 ~/.local/state/dshtui/dshtui.log，轮转 5MB）
  -h, --help        显示本帮助
  -V, --version     显示版本

子命令:
  monitor           Agent Town 监控面板（REQ-009 V0.3）：直连本机 OTR agent-server，
                    2s 轮询 /agents、30s 轮询 /kb-stats，kitty 终端渲染像素小镇。
    --addr <url>    agent-server 地址（默认 {monitor_addr}）

kitty 快捷键（可选，写入 ~/.config/kitty/kitty.conf）:
  map ctrl+shift+a new_tab_with_cwd
  map ctrl+shift+m launch --type=tab --cwd=current dshtui monitor

token 来源优先级: --token <env> > 环境变量 {token_env} > 配置文件 > 交互粘贴
",
        version = env!("CARGO_PKG_VERSION"),
        token_env = DEFAULT_TOKEN_ENV,
        monitor_addr = DEFAULT_MONITOR_ADDR
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_config(content: &str) -> (tempdir::TempDir, std::path::PathBuf) {
        let dir = tempdir::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
    }

    // Minimal temp-dir implementation avoiding the tempfile dependency
    // (unique per-instance path, prevents parallel tests from colliding).
    mod tempdir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        pub struct TempDir(std::path::PathBuf);
        impl TempDir {
            pub fn new() -> Result<Self, std::io::Error> {
                let n = SEQ.fetch_add(1, Ordering::Relaxed);
                let p =
                    std::env::temp_dir().join(format!("dshtui-test-{}-{}", std::process::id(), n));
                std::fs::create_dir_all(&p)?;
                Ok(Self(p))
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn cli_defaults_to_run_action() {
        let cli = parse_cli(["dshtui".to_string()]).unwrap();
        assert_eq!(cli.action, CliAction::Run);
        assert!(cli.url.is_none());
        assert!(cli.token_env.is_none());
    }

    #[test]
    fn cli_parses_url_token_env_and_log() {
        let cli = parse_cli([
            "dshtui".to_string(),
            "--url".into(),
            "http://localhost:3081".into(),
            "--token".into(),
            "MY_DSH_TOKEN".into(),
            "--log".into(),
            "/tmp/dshtui.log".into(),
        ])
        .unwrap();
        assert_eq!(cli.url.as_deref(), Some("http://localhost:3081"));
        assert_eq!(cli.token_env.as_deref(), Some("MY_DSH_TOKEN"));
        assert_eq!(cli.log_file.as_deref(), Some("/tmp/dshtui.log"));
        assert_eq!(cli.action, CliAction::Run);
    }

    #[test]
    fn cli_help_and_version() {
        let cli = parse_cli(["dshtui".to_string(), "--help".into()]).unwrap();
        assert_eq!(cli.action, CliAction::Help);
        let cli = parse_cli(["dshtui".to_string(), "--version".into()]).unwrap();
        assert_eq!(cli.action, CliAction::Version);
        assert!(!usage_text().is_empty());
    }

    #[test]
    fn cli_rejects_unknown_flag() {
        let err = parse_cli(["dshtui".to_string(), "--nope".into()]).unwrap_err();
        assert!(err.contains("未知参数"), "err={err}");
    }

    #[test]
    fn cli_rejects_secret_as_token_arg() {
        // Secret shapes (lowercase/special chars) are rejected — `--token`
        // accepts only environment variable names (D-2).
        let err = parse_cli([
            "dshtui".to_string(),
            "--token".into(),
            "v1.abcDEF123!@#".into(),
        ])
        .unwrap_err();
        assert!(err.contains("环境变量名"), "err={err}");
    }

    #[test]
    fn cli_missing_value_fails() {
        assert!(parse_cli(["dshtui".to_string(), "--url".into()]).is_err());
    }

    #[test]
    fn load_defaults_when_file_missing() {
        let cfg = Config::load(Some(std::path::Path::new("/nonexistent/dshtui.toml"))).unwrap();
        assert_eq!(cfg.server.url, DEFAULT_URL);
        assert_eq!(cfg.perf.window_messages, 200);
        assert_eq!(cfg.perf.page_size, 50);
        assert_eq!(cfg.ui.tick_ms, 33);
    }

    #[test]
    fn load_toml_file() {
        let (_dir, path) = tmp_config(
            r#"
[server]
url = "http://127.0.0.1:3081"
token = "cfg-token"

[ui]
sidebar_width_cells = 24

[perf]
window_messages = 100
"#,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.server.url, "http://127.0.0.1:3081");
        assert_eq!(cfg.server.token, "cfg-token");
        assert_eq!(cfg.ui.sidebar_width_cells, 24);
        assert_eq!(cfg.perf.window_messages, 100);
    }

    #[test]
    fn load_rejects_invalid_toml() {
        let (_dir, path) = tmp_config("not [ valid toml");
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::Toml { .. })
        ));
    }

    #[test]
    fn validate_rejects_zero_window() {
        let (_dir, path) = tmp_config("[perf]\nwindow_messages = 0\n");
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::InvalidValue { .. })
        ));
    }

    #[test]
    fn resolve_precedence_cli_selector_over_env() {
        // Environment variables are process-global state; the precedence
        // matrix must run serially (ordered assertions within one test).
        let _g = EnvGuard::remove(&["DSH_TOKEN", "MY_DSH_TOKEN", "CLI_TOKEN", "NOPE_TOKEN"]);

        // 1) CLI selector takes precedence over DSH_TOKEN.
        std::env::set_var("CLI_TOKEN", "from-cli");
        std::env::set_var("DSH_TOKEN", "from-default-env");
        let cfg = Config::load(Some(std::path::Path::new("/nonexistent/dshtui.toml"))).unwrap();
        let eff = cfg
            .resolve(&Cli {
                token_env: Some("CLI_TOKEN".into()),
                ..Cli::default()
            })
            .unwrap();
        assert_eq!(eff.token.as_deref(), Some("from-cli"));

        // 2) DSH_TOKEN takes precedence over the config file.
        let (_dir, path) = tmp_config("[server]\ntoken = \"cfg-token\"\n");
        let cfg = Config::load(Some(&path)).unwrap();
        let eff = cfg.resolve(&Cli::default()).unwrap();
        assert_eq!(eff.token.as_deref(), Some("from-default-env"));

        // 3) No env var → fall back to the config file token.
        std::env::remove_var("DSH_TOKEN");
        let eff = cfg.resolve(&Cli::default()).unwrap();
        assert_eq!(eff.token.as_deref(), Some("cfg-token"));

        // 4) Nothing at all → token is None (main loop pastes interactively).
        let (_dir2, path2) = tmp_config("");
        let cfg2 = Config::load(Some(&path2)).unwrap();
        let eff2 = cfg2.resolve(&Cli::default()).unwrap();
        assert_eq!(eff2.token, None);

        // 5) The CLI-selected env var does not exist → explicit error
        // (failure scenario: fail-fast).
        let err = cfg2
            .resolve(&Cli {
                token_env: Some("NOPE_TOKEN".into()),
                ..Cli::default()
            })
            .unwrap_err();
        assert!(
            matches!(err, ConfigError::TokenEnvMissing { ref name } if name == "NOPE_TOKEN"),
            "err={err}"
        );

        // 6) Recovery path: after the error, using an existing env var must
        // succeed normally (state not polluted by the old failure).
        std::env::set_var("CLI_TOKEN", "from-cli-again");
        let eff3 = cfg2
            .resolve(&Cli {
                token_env: Some("CLI_TOKEN".into()),
                ..Cli::default()
            })
            .unwrap();
        assert_eq!(eff3.token.as_deref(), Some("from-cli-again"));
    }

    #[test]
    fn cli_url_overrides_config() {
        let (_dir, path) = tmp_config("[server]\nurl = \"http://127.0.0.1:3081\"\n");
        let cfg = Config::load(Some(&path)).unwrap();
        let eff = cfg
            .resolve(&Cli {
                url: Some("http://localhost:9999".into()),
                ..Cli::default()
            })
            .unwrap();
        assert_eq!(eff.url, "http://localhost:9999");
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("http://127.0.0.1:3080"));
        assert!(is_loopback("http://localhost:3080/api"));
        assert!(is_loopback("http://[::1]:3080"));
        assert!(!is_loopback("http://192.168.1.10:3080"));
        assert!(!is_loopback("http://ds.example.com"));
    }

    #[test]
    fn redaction_never_leaks_token() {
        let eff = Effective {
            url: "http://127.0.0.1:3080".into(),
            token: Some("super-secret-token".into()),
            ui: UiConfig::default(),
            perf: PerfConfig::default(),
            monitor: MonitorConfig::default(),
            drafts: DraftsConfig::default(),
            export: ExportConfig::default(),
            keymap: KeymapConfig::default(),
        };
        let s = redact_summary(&eff);
        assert!(!s.contains("super-secret-token"));
        assert!(s.contains("***"));
    }

    // ---------- REQ-009：monitor 子命令 CLI 与配置 ----------

    #[test]
    fn cli_parses_monitor_subcommand() {
        let cli = parse_cli(["dshtui".to_string(), "monitor".into()]).unwrap();
        assert_eq!(cli.action, CliAction::Monitor { addr: None });
        assert_eq!(cli.log_file, None);
    }

    #[test]
    fn cli_parses_monitor_with_addr_and_log() {
        let cli = parse_cli([
            "dshtui".to_string(),
            "monitor".into(),
            "--addr".into(),
            "http://127.0.0.1:9000".into(),
            "--log".into(),
            "/tmp/mon.log".into(),
        ])
        .unwrap();
        assert_eq!(
            cli.action,
            CliAction::Monitor {
                addr: Some("http://127.0.0.1:9000".into())
            }
        );
        assert_eq!(cli.log_file.as_deref(), Some("/tmp/mon.log"));
    }

    #[test]
    fn cli_monitor_rejects_unknown_arg() {
        let err = parse_cli(["dshtui".to_string(), "monitor".into(), "--nope".into()]).unwrap_err();
        assert!(err.contains("未知参数"), "err={err}");
    }

    #[test]
    fn cli_monitor_missing_addr_value_fails() {
        assert!(parse_cli(["dshtui".to_string(), "monitor".into(), "--addr".into()]).is_err());
    }

    #[test]
    fn monitor_config_defaults_and_file_override() {
        let cfg = Config::load(Some(std::path::Path::new("/nonexistent/dshtui.toml"))).unwrap();
        assert_eq!(cfg.monitor.addr, DEFAULT_MONITOR_ADDR);
        assert_eq!(cfg.monitor.poll_agents_ms, 2000);
        assert_eq!(cfg.monitor.poll_kb_ms, 30_000);

        let (_dir, path) = tmp_config(
            r#"
[monitor]
addr = "http://127.0.0.1:8799"
poll_agents_ms = 1000
poll_kb_ms = 15000
"#,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.monitor.poll_agents_ms, 1000);
        assert_eq!(cfg.monitor.poll_kb_ms, 15_000);
    }

    #[test]
    fn monitor_cli_addr_overrides_config() {
        let (_dir, path) = tmp_config("[monitor]\naddr = \"http://127.0.0.1:8888\"\n");
        let cfg = Config::load(Some(&path)).unwrap();
        let eff = cfg
            .resolve(&Cli {
                action: CliAction::Monitor {
                    addr: Some("http://127.0.0.1:9999".into()),
                },
                ..Cli::default()
            })
            .unwrap();
        assert_eq!(eff.monitor.addr, "http://127.0.0.1:9999");
        // 无 CLI 覆盖时取配置文件。
        let eff2 = cfg.resolve(&Cli::default()).unwrap();
        assert_eq!(eff2.monitor.addr, "http://127.0.0.1:8888");
    }

    #[test]
    fn validate_rejects_zero_monitor_poll() {
        let (_dir, path) = tmp_config("[monitor]\npoll_agents_ms = 0\n");
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::InvalidValue { .. })
        ));
        let (_dir2, path2) = tmp_config("[monitor]\npoll_kb_ms = 0\n");
        assert!(matches!(
            Config::load(Some(&path2)),
            Err(ConfigError::InvalidValue { .. })
        ));
    }

    #[test]
    fn usage_text_mentions_monitor_and_kitty() {
        let s = usage_text();
        assert!(s.contains("monitor"), "usage 需含 monitor 子命令");
        assert!(s.contains("kitty.conf"), "usage 需含 kitty 快捷键指引");
        assert!(s.contains(DEFAULT_MONITOR_ADDR));
    }

    /// Test guard that saves and restores environment variables (failure
    /// scenario: leaked env would pollute other tests).
    struct EnvGuard(Vec<(String, Option<String>)>);
    impl EnvGuard {
        fn remove(names: &[&str]) -> Self {
            let saved = names
                .iter()
                .map(|n| (n.to_string(), std::env::var(n).ok()))
                .collect::<Vec<_>>();
            for n in names {
                std::env::remove_var(n);
            }
            Self(saved)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (n, v) in &self.0 {
                match v {
                    Some(v) => std::env::set_var(n, v),
                    None => std::env::remove_var(n),
                }
            }
        }
    }

    // ---------- REQ-005 Step 6：details_width_cells 配置（Notes/04 §1） ----------

    #[test]
    fn details_width_default_is_45() {
        let cfg = Config::default();
        assert_eq!(cfg.ui.details_width_cells, 45, "默认 45 列（Notes/04 §1）");
        let (_dir, path) = tmp_config("[ui]\nshow_turn_rail = true\n");
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.ui.details_width_cells, 45, "老配置缺字段默认兜底");
    }

    #[test]
    fn details_width_is_clamped_to_30_60() {
        let (_dir, path) = tmp_config("[ui]\ndetails_width_cells = 10\n");
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.ui.details_width_cells, 30, "越界小值 clamp 到 30");
        let (_dir, path) = tmp_config("[ui]\ndetails_width_cells = 200\n");
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.ui.details_width_cells, 60, "越界大值 clamp 到 60");
        let (_dir, path) = tmp_config("[ui]\ndetails_width_cells = 36\n");
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.ui.details_width_cells, 36, "合法值原样保留");
    }

    #[test]
    fn effective_carries_details_width_for_injection() {
        let (_dir, path) = tmp_config("[ui]\ndetails_width_cells = 50\n");
        let cfg = Config::load(Some(&path)).unwrap();
        let eff = cfg.resolve(&Cli::default()).unwrap();
        assert_eq!(eff.ui.details_width_cells, 50, "Effective 注入链携带");
    }

    // ---------- REQ-007 V0.4: palette/drafts/export/keymap + ADR-010 atomic
    // write ----------

    #[test]
    fn v04_parses_palette_drafts_export_and_keymap_modes() {
        let (_dir, path) = tmp_config(
            r##"
[ui]
theme = "light"
palette = { accent = "red", unknown_role = "green" }
show_timeline = true

[drafts]
enabled = false
clear = true

[export]
default_dir = "/home/nd/exports"

[keymap.modes.normal]
move_down = "j"
quit = "none"
"##,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.ui.theme, "light");
        assert_eq!(
            cfg.ui.palette.get("accent").map(String::as_str),
            Some("red")
        );
        assert_eq!(cfg.ui.palette.len(), 2, "未知角色保留不 deny");
        assert!(cfg.ui.show_timeline);
        assert!(!cfg.drafts.enabled);
        assert!(cfg.drafts.clear);
        assert_eq!(cfg.export.default_dir, "/home/nd/exports");
        assert_eq!(
            cfg.keymap
                .modes
                .get("normal")
                .unwrap()
                .get("quit")
                .map(String::as_str),
            Some("none")
        );
    }

    #[test]
    fn v04_parses_ui_editor_and_image_soft_limits_d51_d52() {
        // D-51/D-52：`[ui].editor` + max_image_count/max_image_bytes 可解析
        // 且不再是 deny_unknown_fields 拒绝项。
        let (_dir, path) = tmp_config(
            r##"
[ui]
theme = "dark"
editor = "/usr/bin/nano"
max_image_count = 3
max_image_bytes = 10485760
"##,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.ui.editor.as_deref(), Some("/usr/bin/nano"));
        assert_eq!(cfg.ui.max_image_count, 3);
        assert_eq!(cfg.ui.max_image_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn v04_defaults_drafts_enabled_export_empty() {
        let cfg = Config::default();
        assert!(cfg.drafts.enabled);
        assert!(!cfg.drafts.clear);
        assert!(cfg.export.default_dir.is_empty());
        assert!(cfg.ui.palette.is_empty());
        assert!(!cfg.ui.show_timeline);
        // D-51/D-52 默认：数量 10 / 单张 20MiB / editor 无。
        assert_eq!(cfg.ui.max_image_count, 10);
        assert_eq!(cfg.ui.max_image_bytes, 20 * 1024 * 1024);
        assert!(cfg.ui.editor.is_none());
        let eff = cfg.resolve(&Cli::default()).unwrap();
        assert!(eff.drafts.enabled);
        assert_eq!(eff.ui.max_image_count, 10);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_0600_creates_parent_and_sets_mode() {
        let dir = tempdir::TempDir::new().unwrap();
        let target = dir.path().join("sub").join("drafts.toml");
        atomic_write_0600(&target, "drafts = {}\n").unwrap();
        assert!(target.exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "权限 0600");
        let data = std::fs::read_to_string(&target).unwrap();
        assert!(data.contains("drafts"));
        // 无残留 temp 文件。
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "仅目标文件");
    }

    #[test]
    fn save_theme_config_preserves_other_sections() {
        let (_dir, path) = tmp_config(
            r##"
[server]
url = "http://127.0.0.1:3081"

[ui]
theme = "dark"
sidebar_width_cells = 24
"##,
        );
        let palette = {
            let mut m = std::collections::BTreeMap::new();
            m.insert("accent".to_string(), "#123456".to_string());
            m
        };
        save_theme_config(&path, "light", &palette).unwrap();
        let reloaded = Config::load(Some(&path)).unwrap();
        assert_eq!(
            reloaded.server.url, "http://127.0.0.1:3081",
            "其它 section 保留"
        );
        assert_eq!(
            reloaded.ui.sidebar_width_cells, 24,
            "同 section 其它字段保留"
        );
        assert_eq!(reloaded.ui.theme, "light");
        assert_eq!(
            reloaded.ui.palette.get("accent").map(String::as_str),
            Some("#123456")
        );
    }

    #[test]
    fn save_keymap_config_writes_modes_and_keeps_rest() {
        let (_dir, path) = tmp_config("[server]\nurl = \"http://127.0.0.1:3082\"\n");
        let modes = {
            let mut outer = std::collections::BTreeMap::new();
            let mut inner = std::collections::BTreeMap::new();
            inner.insert("move_down".to_string(), "k".to_string());
            outer.insert("normal".to_string(), inner);
            outer
        };
        save_keymap_config(&path, &modes).unwrap();
        let reloaded = Config::load(Some(&path)).unwrap();
        assert_eq!(reloaded.server.url, "http://127.0.0.1:3082");
        assert_eq!(
            reloaded
                .keymap
                .modes
                .get("normal")
                .unwrap()
                .get("move_down")
                .map(String::as_str),
            Some("k")
        );
    }

    #[test]
    fn save_theme_to_missing_file_creates_it() {
        let dir = tempdir::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        save_theme_config(&path, "dark", &std::collections::BTreeMap::new()).unwrap();
        let reloaded = Config::load(Some(&path)).unwrap();
        assert_eq!(reloaded.ui.theme, "dark");
    }
}
