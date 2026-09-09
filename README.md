# dshtui

官方 `dsh web` Remote API 的 Rust TUI 客户端（V1.0.0）。原生单二进制、
ratatui 渲染、vim 风格键位；只读消费本机官方 `dsh web`，不内嵌、不拉起后端进程。

- 认证接入本地官方后端：cookie 仅内存、token 不落盘、secret 不进 argv；
- 三栏布局：workspace/session 侧栏 + 会话转录 + 详情/轨迹（自适应宽度）；
- 窗口化会话历史：snapshot/follow/page 单漏斗合并，seq 去重 + requestId 幂等；
- 富内容：Markdown 渲染、代码高亮、图片（Kitty 协议 + zoom/pager/系统查看器）、上下文 yank；
- 会话操作：composer/steer、stop、搜索、审批（单条 + 列表批量）、export JSONL、
  图片本地软上限、`:edit` 外部编辑器、消息动作（feedback）、模型目录、命令面板；
- 高级面板：Trajectory、subagent 目录、goal/jobs/settings/skills、@ 提及；
- 可观测（V1 REQ-008）：perf 日志（RSS/frame p50·p99/search/page/重连）、
  `dshtui bench` 基准报告、schema-compare 契约对比、契约冒烟测试、空闲停渲染；
- 发布（V1 REQ-008）：CI 门禁（fmt/clippy/test + 契约冒烟）+ cargo-dist tag 预编译发布。

## 功能矩阵

> 里程碑 ↔ 需求映射按仓库分支命名（`task/0NN-dshtui-vXX-*`）：REQ-001/002 = V0.1、
> REQ-003/004 = V0.2、REQ-005/006/009 = V0.3、REQ-007 = V0.4、REQ-008 = V1。

| 里程碑 | 需求 | 关键能力 |
| --- | --- | --- |
| V0.1 核心会话 | REQ-001/002 | 认证接入（cookie 仅内存、token 不落盘、secret 不进 argv）、三栏布局、窗口化会话历史（snapshot/follow/page 单漏斗、seq 去重 + requestId 幂等）、`f` 会话 picker、composer 发送/steer、stop、连接失败启动指引、文件日志 |
| V0.2 搜索/内容/审批 | REQ-003/004 | `/` 结构化搜索、视觉选择/上下文 yank、approval、turn 大纲导航、Markdown 渲染/代码高亮、图片显示（Kitty/zoom/pager/系统查看器）、草稿 |
| V0.3 轨迹/工作区/监控 | REQ-005/006/009 | Trajectory 独立投影、model workspace/命令、模型目录（`M`）、命令面板（`:`）、approval 列表、`dshtui monitor` Agent 监控面板 |
| V0.4 高级会话 | REQ-007 | subagent/父→子发送、goal/jobs/settings/skills、export JSONL（D-46 重建兜底）、消息动作与 feedback、keymap 覆盖、draft 持久化、@ 提及、`:edit` 编辑器链、图片本地软上限、palette |
| V1 NFR/发布收口 | REQ-008 | 性能门禁 `dshtui bench`（JSON 报告）、perf 日志（七字段行）、schema-compare 契约对比、契约冒烟（live + mock）、export JSONL 行格式锁定、空闲停渲染省 CPU、文件日志轮转、CI 门禁 + cargo-dist tag 发布 |

## 快速开始

后端前置：本机已启动官方 `dsh web`（`dsh web --host 127.0.0.1 --port 3080`），并准备 token
（`DSH_TOKEN` 环境变量，或首次启动时在指引屏按 `i` 交互粘贴——raw mode 不回显、不落盘）。

方式一：源码安装（已装 Rust 工具链时）

```bash
cargo install --path .          # 等价: cargo build --release && 把 dshtui 放入 PATH
dshtui --version                # 期望: dshtui 1.0.0
```

方式二：预编译二进制（无需本地编译）

从 GitHub Releases 下载 `x86_64-unknown-linux-gnu` tarball 及对应 `.sha256`，校验后解压安装。V1 当前 `installers = []`，**不发布** `dshtui-installer.sh`；完整命令见 [docs/install.md](docs/install.md)。

## 配置

配置文件：`~/.config/dshtui/config.toml`（示例：`config.example.toml`；复制后修改）。
要点：

```toml
[server]
url = "http://127.0.0.1:3080"   # dsh web 地址（默认仅连 loopback）
token = ""                       # 更推荐环境变量 DSH_TOKEN / 首次启动交互粘贴

[ui]
# theme/sidebar_width_cells/details_width_cells/tick_ms、palette 颜色覆盖、
# editor（:edit 第三级）、max_image_count/max_image_bytes（图片软上限）等

[perf]                           # V1 REQ-008：性能观测
window_messages = 200            # 转录本窗口条数
page_size = 50                   # session/page 每页条数
cache_bytes = 33554432           # 图片/历史缓存上限 32MB
rss_target_mb = 80               # 超限触发 LRU 释放
log = true                       # perf 日志开关（默认开）
log_path = "/tmp/dshtui-perf.log"  # perf 日志路径（空 = 禁用；DSHTUI_PERF_LOG 覆盖）

[log]                            # V1 REQ-008：文件日志轮转
max_bytes = 5242880              # 轮转上限（默认 5MB）

[drafts] / [export] / [monitor]  # 见 config.example.toml 注释
# [keymap]                       # 键位覆盖（mode 内 command 改名/解绑，见 docs 与示例）
```

CLI 覆盖：`--url <url>`、`--token <环境变量名>`（只接受环境变量名，secret 禁止进 argv）、
`--log <file>`。Token 来源优先级：`--token <env>` > 环境变量 `DSH_TOKEN` > 配置 `server.token` >
启动指引屏交互粘贴。

日志：文件日志默认 `~/.local/state/dshtui/dshtui.log`（5MB 轮转，`[log] max_bytes` 可调）；
perf 行默认 `/tmp/dshtui-perf.log`（每 ~5s 追加一行，见下）。错误与 tracing 全进文件，不刷屏终端。

## 键位速查

### 主 TUI（NORMAL 模式）

| 键 | 动作 |
| --- | --- |
| `j` / `k` | 上/下滚动（浏览中冻结 tail） |
| `Ctrl+d` / `Ctrl+u` | 半页滚动 |
| `G` / `gg` | 跳到末尾 / 开头 |
| `f` | 会话 fuzzy picker（nucleo 匹配；Enter 打开、Esc 关闭） |
| `i` | 进入 composer（INSERT） |
| `?` | 帮助 overlay（`?`/`Esc` 关闭；help 内 `q` 直接退出） |
| `q` / `Ctrl+c` | 退出（运行中会话先确认，再请求 stop 后恢复终端） |
| `o` | 打开选中会话 / 焦点块 |
| `s` | 停止运行中会话 |
| `r` | 启动失败时重试探测 |
| `h` / `l` | 折叠全部项目 / 展开全部项目 |
| `Enter` | 打开焦点块（图片占位 → ImageView / 系统查看器） |
| `Ctrl+w` | 焦点循环（侧栏/转录/详情） |
| `/` | 结构化搜索 overlay（`n`/`N` 巡览窗口命中） |
| `v` / `V` | 视觉选择（字符/行），`y` 复制 |
| `y` | 上下文 yank（代码块/链接/图片/工具结果/段落） |
| `O` | turnOutline 大纲列表 |
| `]` / `[` | 跳下一 / 上一轮 |
| `1` / `2` | Chat / Trajectory tab（`gt`/`gT` 同义；`gv` 循环侧栏视图） |
| `M` | 模型目录 overlay |
| `:` | 命令面板（`:settings`/`:export`/`:subagents`/`:goal`/`:jobs`/`:skills`/…） |
| `m` | 消息动作菜单（焦点消息行；rating/feedback 等） |

### 常用模态内键位

- **Approval（审批弹窗）**：`y` 允许 / `n` 拒绝 / `a` 始终允许 / `L` 审批列表 / `q`/`Esc` 取消。
- **ApprovalList**：`j`/`k` 移动、`r` 重试失败项、`A` 批量 allowed-once、`y`/`n` 单条、`q` 返回。
- **Trajectory**：`j`/`k` 事件行上下、`z` 折叠、`d`/`Enter` 详情、`/` 过滤、`y` 复制、`q` 退出。
- **ImageView**：`o` 系统查看器打开、`y` 复制路径、`]`/`[` 同消息多图翻页、`+`/`-`/`0` zoom、`q` 关闭。
- **Goal 面板**：`c` create / `e` edit / `p` pause / `r` resume / `x` complete / `d` clear。
- 键位可覆盖：`[keymap.modes.<mode>]` 改键或 `none` 解绑（非法项警告并保默认），见 `config.example.toml`。

### Agent 监控面板（`dshtui monitor`）

```bash
dshtui monitor [--addr http://127.0.0.1:8799] [--log <file>]
```

- 数据源：直连本机 OTR agent-server（默认 `127.0.0.1:8799`）——`GET /agents` 2s 轮询、
  `GET /kb-stats` 30s、`POST /agent/chat` 一问一答；零后端改动。
- Kitty 终端渲染像素小镇（静态背景只传一次，脏矩形增量，静态停帧）；非 Kitty 自动降级文本 roster。
- 键位：`j`/`k` 焦点、`gg`/`G` 首尾、`Enter` 详情、`c` 问答、`f` 加油、`l` 定位、`s` KB 统计、
  `/` 过滤、`?` 帮助、`q` 退出。
- 不可达：启动指引 + 指数退避重试（不自动拉起 agent-server），恢复自动续。

## 子命令与工具（V1 REQ-008）

### `dshtui bench` —— 性能门禁基准（V1 REQ-008 新增）

```bash
dshtui bench [--report <path>] [--fixture auto|live|seed] [--scenario <name>] [--log <file>]
```

- 默认产出 JSON 报告到 `target/perf/perf-report.json`（`--report` 可改路径）；
- `--fixture`：`auto`（默认）/ `live`（连真实 dsh web）/ `seed`（确定性种子数据）；
- `--scenario <name>`：运行指定场景（如 search/page/export/长会话渲染等，具体场景名以
  `dshtui bench --help` 为准）；
- 报告字段对齐 perf 口径（RSS、帧 p50/p99、search/page 耗时、重连计数等），供 CI 性能门禁判定。

### 契约冒烟（V1 REQ-008 新增）

官方 `dsh web` 升级后跑：`tests/live_smoke.rs`（连本机真实 dsh web，需 `DSH_TOKEN` + 运行中的
后端，ignored 默认；外加 `tests/api_protocol.rs`、`tests/export_rebuild_proto.rs` 等离线 mock）。
具体命令见 [docs/install.md](docs/install.md) 与 [docs/contributing.md](docs/contributing.md)。

### schema-compare —— 官方协议 schema 对比（V1 REQ-008）

对比官方 `dsh web` 客户端两侧 `typert.remote-client.js`（zod codec bundle，由
`@deepseek-ai/dsh-typert-generator` 生成）的结构，产出 `added/removed/changed`
的 SchemaDiff JSON（`schema_version: 1`）：

```bash
node scripts/schema-compare.mjs --from <旧版 dir-or-file> --to <新版 dir-or-file> \
    [--from-version X] [--to-version Y] [--out <path>]
```

- 输入：单个 `typert.remote-client.js` 或含此类文件的目录（递归收集）；
- 版本号缺省读就近 `package.json` 的 version；
- 不指定 `--out` 时 SchemaDiff JSON 打到 stdout；指定时原子写；
- 解析失败/输入无效 → stderr 报错 + exit 1，不产出半成品文件。
- 完整参数见 `node scripts/schema-compare.mjs --help`。

### perf 日志行（七字段，V1 REQ-008）

主 TUI 每帧采样、每 ~5s 追加一行（`[perf] log` 默认开；`DSHTUI_PERF_LOG` 覆盖路径）：

```
ts=<unix-ms> rss_kb=<KB> frame_ms_p50=<..> frame_ms_p99=<..> [search_ms=<..>] [page_latency_ms=<..>] ws_reconnects=<n>
```

七字段：`ts_ms` / `rss_kb` / `frame_ms_p50` / `frame_ms_p99` / `search_ms`（可选） /
`page_latency_ms`（可选）/ `ws_reconnects`（单调计数）。未采样到的可选字段不写该 key。

## 升级兼容流程

官方 `dsh web` 发布新版本时，按以下顺序验证兼容（V1 REQ-008 收口流程）：

1. **schema-compare**：`node scripts/schema-compare.mjs --from <旧> --to <新>`
   （可加 `--from-version`/`--to-version`/`--out`）对比新旧两侧
   `typert.remote-client.js` 结构，识别破坏性字段变更；
2. **契约冒烟**：对变更面跑 mock 协议测试（离线、全量、快），再跑 `tests/live_smoke.rs`
   连真实后端冒烟；
3. **修复 SLA**：发现不兼容在 **24 小时内** 修复并合入（export JSONL 行格式已锁定，
   变更需走 schema 对比 + 契约测试更新）。

## 测试质量

```bash
cargo test --all-targets                 # 全量测试（api_protocol/ui_golden/keymap/model/
                                         #   monitor_protocol/export_rebuild_proto 等）
cargo test --all-targets -- --ignored     # 含需要真实后端的 live_smoke（需 DSH_TOKEN + dsh web）
cargo clippy --all-targets --all-features -- -D warnings   # 零告警门禁
cargo fmt --all -- --check                # 格式门禁
```

集成测试在 `tests/`（协议 mock HTTP/WS、模型、keymap、TestBackend golden、export 重建 proto），
命名风格：`api_protocol` / `ui_golden` / `keymap` / `model` / `monitor_protocol` /
`export_rebuild_proto`。CI 门禁（fmt/clippy/test + 契约冒烟）见 `.github/workflows/ci.yml`；
tag 发布（cargo-dist 预编译二进制）见 `.github/workflows/release.yml`（两者随 V1 Step 7 落地）。

## 文档导航

| 文档 | 内容 |
| --- | --- |
| [docs/install.md](docs/install.md) | 安装（源码 / 预编译 + checksum 校验）、`DSH_TOKEN`、连接本机 dsh web |
| [docs/comparison-dsh-tui.md](docs/comparison-dsh-tui.md) | 与 `@deepseek-harness-tui/dsh-tui`（Node/Ink TUI）对比 |
| [docs/architecture.md](docs/architecture.md) | 架构分层图与说明、ADR 边界、REQ-008 观测面 |
| [docs/contributing.md](docs/contributing.md) | 环境、测试、代码风格、commit 规范、评审流程 |
| `config.example.toml` | 配置示例（含注释） |
