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
}

/// Full structure of `~/.config/dshtui/config.toml` (Notes/02 §7 draft).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub ui: UiConfig,
    pub perf: PerfConfig,
    pub keymap: KeymapConfig,
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
    pub sidebar_width_cells: u16,
    /// 右栏详情列宽（REQ-005 V0.3，Notes/04 §1：45 默认，30–60 可调；
    /// clamp 在 `validate` 执行）。
    pub details_width_cells: u16,
    pub show_turn_rail: bool,
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

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct KeymapConfig {}

/// Effective merged config (CLI overrides file).
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub url: String,
    pub token: Option<String>,
    pub ui: UiConfig,
    pub perf: PerfConfig,
}

pub const DEFAULT_URL: &str = "http://127.0.0.1:3080";
pub const DEFAULT_TOKEN_ENV: &str = "DSH_TOKEN";
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
            sidebar_width_cells: DEFAULT_SIDEBAR_WIDTH,
            details_width_cells: DEFAULT_DETAILS_WIDTH_CELLS,
            show_turn_rail: false,
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

        Ok(Effective {
            url,
            token,
            ui: self.ui.clone(),
            perf: self.perf.clone(),
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

/// Parse CLI args. `--token` accepts only an environment variable name; the
/// secret never enters argv.
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

选项:
  --url <url>       dsh web 地址（默认 http://127.0.0.1:3080）
  --token <env>     环境变量名选择器（默认 {token_env}），secret 不出现在 argv
  --log <file>      日志文件路径（默认 ~/.local/state/dshtui/dshtui.log，轮转 5MB）
  -h, --help        显示本帮助
  -V, --version     显示版本

token 来源优先级: --token <env> > 环境变量 {token_env} > 配置文件 > 交互粘贴
",
        version = env!("CARGO_PKG_VERSION"),
        token_env = DEFAULT_TOKEN_ENV
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
        };
        let s = redact_summary(&eff);
        assert!(!s.contains("super-secret-token"));
        assert!(s.contains("***"));
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
}
