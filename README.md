# dshtui

Official `dsh web` Remote API 的 Rust TUI 客户端（V0.1 核心底座）。

- 认证接入本地官方后端（cookie 仅内存，token 不落盘）；
- 三栏布局：workspace/session 侧栏 + 会话转录 + 详情（自适应宽度）；
- 窗口化会话历史：snapshot/follow/page 单漏斗合并，seq 去重 + requestId 幂等；
- `nucleo` 会话 picker（`f`），vim 风格导航，状态条只读官方 projections；
- 不内嵌、不拉起 `dsh web`；连接失败显示启动指引（`dsh web --host 127.0.0.1 --port 3080`）。

## 构建与运行

```bash
cargo build --release          # 或 cargo run --
./target/release/dshtui --help
```

## 配置

- 配置文件：`~/.config/dshtui/config.toml`（示例：`config.example.toml`）。
- 后端默认 `http://127.0.0.1:3080`，仅建议 loopback。
- Token 来源优先级：`--token <环境变量名>` > `DSH_TOKEN` > 配置 `server.token` >
  启动指引屏按 `i` 交互粘贴（raw mode 不回显、不写 history、不落盘）。
  `--token` 只接受**环境变量名**，secret 禁止直接放入命令行。
- 日志：`--log <file>`（默认 `~/.local/state/dshtui/dshtui.log`，5MB 轮转，
  错误与 tracing 全部进文件日志，不刷屏终端）。

## 键位（V0.1）

| 键 | 动作 |
|----|------|
| `j` / `k` | 上/下滚动（浏览中冻结 tail） |
| `Ctrl+d` / `Ctrl+u` | 半页滚动 |
| `G` / `gg` | 跳到末尾 / 开头 |
| `f` | 会话 fuzzy picker（nucleo 匹配，Enter 打开，Esc 关闭） |
| `h` / `l` | 折叠全部项目 / 展开全部项目 |
| `o` | 打开当前选中会话 |
| `?` | 帮助 overlay |
| `i` | 输入（启动指引屏为粘贴 token；REQ-002 起为 composer） |
| `s` | 停止运行中会话 |
| `r` | 启动失败时重试探测 |
| `q` / `Ctrl+c` | 退出（运行中会话先确认，再请求 stop 后恢复终端） |

## Agent 监控面板（V0.3，REQ-009）

把 obsidian-task-runner agent-server 的 HTML「Agent Town」搬到 Kitty tab 的 Rust
TUI 像素小镇：

```bash
dshtui monitor [--addr http://127.0.0.1:8799] [--log <file>]
```

- 数据源：直连本机 OTR agent-server（默认 `127.0.0.1:8799`）——
  `GET /agents` 2s 轮询（`x-agents-finished` 完工计数）、`GET /kb-stats` 30s、
  `POST /agent/chat` 一问一答（同 agent 多轮复用 sessionId）；零后端改动。
- Kitty 终端：960×540 像素小镇（四季色板/昼夜光影/STAGE→职业建筑/装饰居民/
  A* 寻路），静态背景只传一次，后续帧 kitty `a=f` 脏矩形增量（20–30fps 预算，
  静态停帧）；鼠标点击 NPC 开详情（幂等）。
- 非 Kitty 终端：自动降级纯文本 roster，功能键位不变。
- 键位：`j/k` 焦点、`gg/G` 首尾、`Enter` 详情、`c` 问答、`f` 加油、`l` 定位、
  `s` KB 统计、`/` 过滤、`q` 退出、`?` 帮助。
- 不可达：启动指引 + 指数退避重试（不自动拉起 agent-server），恢复自动续。

kitty 快捷键（可选，写入 `~/.config/kitty/kitty.conf`，非程序职责）：

```conf
map ctrl+shift+m launch --type=tab --cwd=current dshtui monitor
```

配置段（`~/.config/dshtui/config.toml`）：

```toml
[monitor]
addr = "http://127.0.0.1:8799"
poll_agents_ms = 2000
poll_kb_ms = 30000
```

性能测量（Notes/06 §8 口径）：`DSHTUI_PERF_LOG=/tmp/dshtui-perf.log dshtui monitor`
每 5s 追加一行 `rss_mb / frame_p50_ms / poll_ms`。

## 测试与质量

```bash
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

集成测试在 `tests/`（协议 mock HTTP/WS、模型、keymap、TestBackend golden）。
