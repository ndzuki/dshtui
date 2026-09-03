//! dshtui 入口：CLI 解析 → 配置加载 → token 解析 → tracing 初始化 → TUI 主循环。
//! 遵循 ADR-001（不自动拉起 dsh web）、D-2（`--token` 为环境变量名选择器）。

use std::process::ExitCode;

use dshtui::config::{self, Cli, CliAction};

fn main() -> ExitCode {
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

    // 非 loopback 地址显式警告（REQ-001 §7 B01–B03）。
    if !config::is_loopback(&eff.url) {
        eprintln!(
            "警告: 目标地址 {} 不是 loopback，认证 cookie 将发送到非本机地址，请确认这是你信任的服务器。",
            eff.url
        );
    }

    // token 缺失时由 TUI 主循环交互粘贴（不回显）；此处打印脱敏摘要即可。
    println!("[dshtui] 配置就绪: {}", config::redact_summary(&eff));

    // TUI 主循环在 Step 4/7 接线；Step 1 只交付可编译的配置/CLI 基线。
    ExitCode::SUCCESS
}
