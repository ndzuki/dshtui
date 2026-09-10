# 对比：dshtui（Rust） vs `@deepseek-harness-tui/dsh-tui`（Node/Ink）

> 素材来源：
> - dshtui：本仓库源码/Cargo.toml/README（1.0.0）。
> - dsh-tui：本机 `~/.dsh/profiles/dsh-tui/node_modules/@deepseek-harness-tui/dsh-tui@0.9.3`
>   的 package.json / README / `bin/dsh-tui.js`（2026-09-08 读取；其文档目录 docs/ 未随包分发，
>   未读到的内容一律标「未实测/待核对」，不臆测）。

## 一句话结论

两者都是 DeepSeek Harness 生态的终端交互层，但**形态与接入面不同**：dshtui 是
独立的 Rust 原生二进制，只读消费**官方 `dsh web` 的 Remote API**（web 产品后端，
HTTP + WS mux）；dsh-tui 是 **Node/Ink 写的、挂在 deepseek-harness CLI（`dsh`）上的
前端壳/插件**，随 agent 进程运行。选型取决于你要接哪一个后端面。

| 维度 | dshtui（本仓库） | @deepseek-harness-tui/dsh-tui | 判断依据 |
| --- | --- | --- | --- |
| 版本/状态 | 1.0.0（V1） | 0.9.3，public beta | dshtui Cargo.toml（Step 7 升 1.0.0）；dsh-tui package.json/README badge |
| 技术栈 | Rust：ratatui 0.29 + crossterm + tokio + reqwest；单一原生二进制 | Node.js（引擎 `^22.19` 或 `>=24`）+ React 19 + react-reconciler（移植 Ink 内核）+ chalk/marked/highlight.js 等 | dshtui Cargo.toml；dsh-tui package.json `dependencies`/`engines`/README「ported Ink core」 |
| 形态与集成面 | **独立客户端进程**，连接官方 dsh web Remote API（loopback HTTP/WS），不内嵌不拉起后端（ADR-001） | **dsh CLI 的 TUI 插件**：`dsh-tui`/`dst` 双态启动器 → `dsh --profile dsh-tui`，profile 内副本承载完整逻辑，可 `dsh plugin --profile dsh-tui add @deepseek-harness-tui/dsh-tui` 安装 | dshtui README/`src/api`（DshClient.connect，绝不 spawn 后端）；dsh-tui package.json `bin` 与 `bin/dsh-tui.js` 头部注释（delegating launcher、profile 委托、`dsh plugin` 自举） |
| 运行时依赖 | 仅二进制；需要本机官方 `dsh web`（默认 127.0.0.1:3080）在跑 | Node ≥22.19 + `@deepseek-ai/dsh`（deepseek-harness CLI）+ profile 环境（`$DSH_HOME/profiles/dsh-tui`） | dshtui README；dsh-tui package.json `engines`、README 快速开始（`npm i -g @deepseek-ai/dsh @deepseek-harness-tui/dsh-tui`） |
| 安装 | cargo/rustup 源码安装或 cargo-dist 预编译（见 docs/install.md）；单二进制进 PATH | npm 全局 + plugin/profile 挂载（dsh 侧）；另有 VS Code 扩展形态 | docs/install.md；dsh-tui README 快速开始与 docs 索引（vscode.md） |
| 功能覆盖 | 官方 `dsh web` Remote API 的**只读客户端能力子集**：workspace/session 窗口化历史、搜索、composer/steer、stop、审批（含列表批量）、export JSONL、图片、Trajectory、subagent/goal/jobs/settings/skills 等面板 | deepseek-harness **agent 会话面的 Claude Code 风格前端**：像素鲸鱼顶栏/工作状态行/TPS 与上下文进度条展示、`/resume` `/new` `/compact` `/export` `/btw`、模型热切换、原生 subagent、会话 fork、自动更新、思考流式展开/双击 Esc 时间回溯、插件生态（browser/computer use 等附属扩展） | 本仓库功能矩阵与 dsh-tui README「核心能力」/`docs/interaction.md` 索引（未随包分发，标「README 声明、未实测」） |
| 与官方 web 的关系 | 直接消费官方 dsh web 的 Remote API（协议层 envelope/mux，字段/状态机以官方契约为准） | 不经过官方 dsh web；直接驱动本地 `dsh` agent 进程（profile 的 sandbox/approval 策略由 DSH profile 提供） | dsh-tui README「权限与安全边界」：不实现独立沙箱，用 DSH profile 策略；dshtui ADR-001 |
| 资源占用/启动延迟 | 未在本环境对同一负载做 A/B 实测。方法：`/usr/bin/time -v dshtui` 测 RSS/启动到首帧；dshtui 自带 perf 日志（rss_kb/frame p50/p99）可直接量化。理性预期：静态 Rust 二进制无 Node runtime/依赖树/JIT warmup，资源与启动开销更低——**属预期，需实测数据支撑，不写成结论** | 未实测；Node runtime + React/Ink 渲染树常驻，npm 依赖较多（package.json dependencies ≥27 项） | 各自 package/Cargo 事实可查；数值均未实测 |
| 降级路径 | 明确：非 Kitty 终端自动降级；后端不可达 → 启动指引 + 指数退避重试；endpoint 不可用 → 本地降级（feedback 本地标记、export JSONL 重建、permission 类不自动重试）；错误分类 404/5xx/transport 与 401/403 可区分 | 未实测/待核对：其 docs/architecture.md「已知限制」未随包分发，README 仅声明非沙箱、Windows 无对应 sandbox 后端时退回 danger-full-access | dshtui 源码（ClientError::HttpStatus 分类、StartupFailed 指引）；dsh-tui README「权限与安全边界」 |
| 性能可度量性 | 一等公民（V1 REQ-008）：`perf.rs` FrameSampler（p50/p99）、PerfLogEntry 七字段 perf 日志、`dshtui bench` JSON 基准报告、schema-compare 契约对比、契约冒烟（live+mock），且 CI 门禁化 | 有 TPS/上下文进度条等**展示性仪表**（agent 侧指标），未见随包分发的基准/契约工具；渲染侧（Ink）性能无本地可复现基准 | dshtui src/perf.rs、README 子命令节；dsh-tui README 功能声明（未实测） |
| 升级兼容流程 | 显式：官方 dsh web 新版本 → schema-compare → 契约冒烟 → 24h 修复 SLA；export JSONL 行格式锁定 | /update 自动更新（版本随 profile 前进）；升级对官方协议 schema 的防御未见文档（待核对） | README「升级兼容流程」；dsh-tui `bin/dsh-tui.js` 注释（/update 迁移启动器） |

## 提示

- 上面两套「功能覆盖」面不同，**不要直接按功能数量对表**。若你的「官方 web 能力子集」
  指 dsh web（Remote API / web 产品）提供的会话/搜索/审批/export 等，那是 dshtui 的领域；
  若指 deepseek-harness CLI agent 的本地会话与插件能力，那是 dsh-tui 的领域。
- 凡标「未实测/待核对」的数值类/文档类结论，需要实测或官方文档佐证后再写进对外材料。
- 二者并非互斥：可在同一机器上并存（dshtui 连 dsh web；dsh-tui 挂 dsh profile），
  接入面不同、互不依赖。
