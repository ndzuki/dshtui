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
  `dshtui bench` 基准（11 指标 + JSON/md 双报告）、schema-compare + snapshot golden、
  export golden 离线回归、契约冒烟（live + mock + 官方 alpha 实例）、空闲停渲染；
- 升级兼容（V1 REQ-008）：`upgrade-signal` workflow（每日检测官方新 alpha → 24h SLA
  tracking issue + 自动契约冒烟）；
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
| V1 NFR/发布收口 | REQ-008 | 性能门禁 `dshtui bench`（11 指标 + JSON/md 双报告 + 10k scale + live verified）、perf 日志（七字段行）、schema-compare + snapshot golden、契约冒烟（live + mock + 官方 alpha 实例）、export golden 离线回归、export JSONL 行格式锁定、空闲停渲染省 CPU、文件日志轮转、升级信号 workflow（24h SLA）、CI 门禁 + cargo-dist tag 发布 |

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
dshtui bench [--report <path>] [--report-md <path>] [--fixture auto|live|seed] [--scenario <name>]
```

- 进程内 headless（TestBackend）测量 **11 项 AC-008 指标**，双产物**原子写**：
  JSON（机器 schema `target/perf/perf-report.json`）+ markdown（人读表
  `target/perf/perf-report.md`）；`--report`/`--report-md` 可改路径（md 空 =
  跳过；任一写失败 exit 1 fail-closed）；
- `--fixture`：`auto`（默认——本机 3080 可达且设 `DSH_TOKEN` → 读真实会话列表做
  live verified；否则确定性 seed 兜底并标注 `seed-fallback`）/ `live`（强制 live，
  缺 token/不可达也 seed 兜底不硬失败）/ `seed`（强制确定性，CI 用）；
- `--scenario <name>`：只跑单个场景（默认全量）。场景名（=报告 metric）：
  `startup_ms` `first_screen_ms` `search_ms` `scroll_frame_p99_ms` `scroll_fps`
  `page_flip_ms` `list_10k_search_ms` `list_10k_first_screen_ms` `idle_rss_mb`
  `stream_rss_mb` `image_rss_mb`；
- 阈值（REQ 固定，见 `src/bench.rs` THRESHOLDS）：启动 <1000ms、列表首屏 <300ms、
  搜索 <30ms、滚动 p99 <33ms、滚动 fps ≥15、翻页 <33ms、1 万会话搜索 <30ms/
  首屏 <300ms、RSS 空闲 <25MB/流式 <80MB/图片密集 <150MB；退出码 0=全 PASS /
  1=有 FAIL / 2=全 skip（under-scale）。

### 契约冒烟（V1 REQ-008 新增）

官方 `dsh web` 升级后跑：`tests/live_smoke.rs`（连本机真实 dsh web，需 `DSH_TOKEN` + 运行中的
后端，ignored 默认；外加 `tests/api_protocol.rs`、`tests/export_rebuild_proto.rs` 等离线 mock）。
`tests/live_alpha_smoke.rs` 是**数据无关协议表面**冒烟（认证/list envelope/modelCatalog/
错误信封/export 路由），由 `scripts/ci-live-smoke.sh` 对官方 alpha 一次性只读实例自动跑。
具体命令见 [docs/install.md](docs/install.md) 与 [docs/contributing.md](docs/contributing.md)。

### schema-compare —— 官方协议 schema 对比 + snapshot golden（V1 REQ-008）

对比官方 `dsh web` 客户端两侧 `typert.remote-client.js`（zod codec bundle，由
`@deepseek-ai/dsh-typert-generator` 生成）的结构，产出 `added/removed/changed`
的 SchemaDiff JSON（`schema_version: 1`）。支持 **bundle 直 diff** 与
**schema snapshot golden diff**（D-63）：

```bash
# bundle vs bundle
node scripts/schema-compare.mjs --from <旧版 dir-or-file> --to <新版 dir-or-file> \
    [--from-version X] [--to-version Y] [--out <path>]

# snapshot vs snapshot（或与 bundle 混用）
node scripts/schema-compare.mjs --from-snapshot <a.json> --to-snapshot <b.json> [--out <path>]

# 导出规范 schema 快照（golden 入库；确定性字节输出）
node scripts/schema-compare.mjs --mode snapshot --bundle <dir-or-file> --version X \
    --out schemas/dsh-api-schema-X.json
```

- 已入库 golden：`schemas/dsh-api-schema-0.1.2-rc.1.json`（官方 0.1.2-rc.1，
  16 namespace/74 method/183 entry，确定性）；
- 输入：单个 `typert.remote-client.js`、含此类文件的目录，或 schema 快照 JSON；
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

1. **升级信号**（D-64）：`upgrade-signal` workflow 每日 + 手动检测官方 `@deepseek-ai/dsh`
   新 alpha（vs last-known-good `0.1.2-rc.1`）——发现变化自动开 24h SLA tracking issue
   并触发 `scripts/ci-live-smoke.sh`（下载该 alpha → 隔离起只读实例 → 数据无关契约
   表面冒烟，无需仓库 secret）；也可手动 workflow_dispatch 勾选 `upgrade_alpha` 跑同一
   冒烟。
2. **schema-compare**：`node scripts/schema-compare.mjs --from-snapshot
   schemas/dsh-api-schema-0.1.2-rc.1.json --to <新版 bundle 目录>`（或先
   `--mode snapshot` 导出新 golden）对比结构，识别破坏性字段变更。
3. **export golden 复核**（D-65）：`scripts/live-export-lock.sh --golden`
   （或离线 `scripts/export-golden-check.sh`）对新版本官方 export 行格式复核；
   有变化更新 `fixtures/export-golden/v<版本>/`。
4. **契约冒烟**：对变更面跑 mock 协议测试（离线、全量、快），再跑 `tests/live_smoke.rs`
   连真实后端冒烟。
5. **修复 SLA**：发现不兼容在 **24 小时内** 修复并合入（export JSONL 行格式已锁定，
   变更需走 schema 对比 + 契约测试更新）。

## 测试质量

```bash
cargo test --all-targets                 # 全量测试（api_protocol/ui_golden/keymap/model/
                                         #   monitor_protocol/export_rebuild_proto 等）
cargo test --all-targets -- --ignored     # 含需要真实后端的 live_smoke（需 DSH_TOKEN + dsh web）
cargo clippy --all-targets --all-features -- -D warnings   # 零告警门禁
cargo fmt --all -- --check                # 格式门禁
```

集成测试在 `tests/`（协议 mock HTTP/WS、模型、keymap、TestBackend golden、export 重建 proto、
live_smoke / live_alpha_smoke 真实/隔离实例冒烟），命名风格：`api_protocol` / `ui_golden` /
`keymap` / `model` / `monitor_protocol` / `export_rebuild_proto` / `live_smoke` /
`live_alpha_smoke`。CI 门禁（fmt/clippy/test + 契约冒烟 + upgrade-contract-smoke 可选）见
`.github/workflows/ci.yml`；官方升级信号 + 24h SLA tracking 见 `.github/workflows/upgrade-signal.yml`；
tag 发布（cargo-dist 预编译二进制）见 `.github/workflows/release.yml`。

## 文档导航

| 文档 | 内容 |
| --- | --- |
| [docs/install.md](docs/install.md) | 安装（源码 / 预编译 + checksum 校验）、`DSH_TOKEN`、连接本机 dsh web、升级后契约冒烟 |
| [docs/comparison-dsh-tui.md](docs/comparison-dsh-tui.md) | 与 `@deepseek-harness-tui/dsh-tui`（Node/Ink TUI）对比 |
| [docs/architecture.md](docs/architecture.md) | 架构分层图与说明、ADR 边界、REQ-008 观测面 |
| [docs/contributing.md](docs/contributing.md) | 环境、测试、代码风格、commit 规范、评审流程 |
| `config.example.toml` | 配置示例（含注释） |
| `schemas/dsh-api-schema-0.1.2-rc.1.json` | 官方协议 schema snapshot golden（D-63） |
| `fixtures/export-golden/v0.1.2-rc.1/` | export JSONL golden 字节 fixture（D-65，离线回归） |
