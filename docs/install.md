# 安装指南

dshtui 提供两条安装路径：**源码安装**（Rust 工具链，任何平台）与**预编译二进制**
（cargo-dist releases 产物，无需本地编译）。装完统一用 `dshtui --version` 验证。

- 当前版本：**1.0.0**（V1）
- 仓库：<https://github.com/ndzuki/dshtui>
- MSRV：`rust-version = "1.80"`（建议 stable 最新）

## 前置：官方 dsh web

dshtui 是官方 `dsh web` Remote API 的**客户端**，不内嵌、不拉起后端。使用前请确保：

1. 本机已启动官方 `dsh web`（默认地址 `http://127.0.0.1:3080`）；
2. 有一个可用的访问 token，放在环境变量 `DSH_TOKEN` 中（推荐），或首次启动时在指引屏
   按 `i` 交互粘贴（raw mode 不回显、不写 history、不落盘）。

> 安全边界：默认仅连 loopback；`--url`/配置指向非 loopback 地址时会显式告警。
> token 只进内存 cookie，不落盘；`--token` 只接受**环境变量名**，secret 禁止进命令行。

## 路径一：源码安装（cargo / rustup）

### 1. 安装 Rust（rustup stable）

```bash
# 未装 rustup 时（https://rustup.rs）：
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 然后重新登录 shell 或 source 环境：
#   . "$HOME/.cargo/env"
rustc --version      # 期望 ≥ 1.80
cargo --version
```

### 2. 克隆并安装

```bash
git clone https://github.com/ndzuki/dshtui.git
cd dshtui

# 方式 A：装到 ~/.cargo/bin（推荐）
cargo install --path . --locked

# 方式 B：只构建不安装，直接用产物
cargo build --release --locked
./target/release/dshtui --version
```

`cargo install --path .` 会把 `dshtui` 放进 `~/.cargo/bin`（确保该目录在 `PATH`）。

### 3. 验证

```bash
dshtui --version        # 期望: dshtui 1.0.0
dshtui --help
```

## 路径二：预编译二进制（cargo-dist releases）

每个 tag 发布由 `.github/workflows/release.yml`（cargo-dist）产出预编译产物到 GitHub
Releases（`https://github.com/ndzuki/dshtui/releases`）。

### 1. 手动下载 + checksum 校验

> V1 当前 cargo-dist 配置为 `installers = []`，只发布归档与校验和，不生成
> `dshtui-installer.sh`。以后若启用 installer，必须先同步 Cargo.toml、release workflow
> 与本安装文档。

cargo-dist 产物命名形如：

```
dshtui-v1.0.0-<target>.tar.gz          # 例如 x86_64-unknown-linux-gnu
dshtui-v1.0.0-<target>.tar.gz.sha256
```

```bash
# 以 Linux x86_64 为例；macOS 用 aarch64-apple-darwin / x86_64-apple-darwin，
# Windows 用 .zip（实际产物集以该 release 的 assets 为准）
VER=v1.0.0
TGT=x86_64-unknown-linux-gnu
ASSET=dshtui-${VER}-${TGT}.tar.gz
curl -LOf "https://github.com/ndzuki/dshtui/releases/download/${VER}/${ASSET}"
curl -LOf "https://github.com/ndzuki/dshtui/releases/download/${VER}/${ASSET}.sha256"

# checksum 校验（比对输出中哈希一致）
sha256sum -c "${ASSET}.sha256"

# 解压并安装
tar -xzf "${ASSET}"
sudo mv dshtui-${VER}-${TGT}/dshtui /usr/local/bin/   # 或放 ~/.local/bin 并在 PATH 中
dshtui --version        # 期望: dshtui 1.0.0
```

> `sha256sum` 在 macOS 上为 `shasum -a 256`。校验步骤**不要省略**：请只运行通过
> checksum 校验的二进制。

## 配置与连接本机 dsh web

### 1. 配置文件（可选）

```bash
mkdir -p ~/.config/dshtui
cp config.example.toml ~/.config/dshtui/config.toml
# 按需编辑（server.ui/perf/log/keymap/drafts/export 等段）
```

缺省无需配置即可运行（默认 `http://127.0.0.1:3080` + `DSH_TOKEN`）。

### 2. 设置 token

```bash
export DSH_TOKEN='<你的 token>'        # token 来源优先级最高的非交互方式
```

### 3. 启动

```bash
dshtui
```

- 后端可达：进入主界面（workspace/session 侧栏 + 转录）。
- 后端不可达：显示启动指引（`dsh web --host 127.0.0.1 --port 3080`），启动后按 `r` 重试，
  或先 `dsh web --host 127.0.0.1 --port 3080` 再启动 dshtui。

### 4. 常见环境

| 场景 | 说明 |
| --- | --- |
| 自定义后端地址 | `dshtui --url http://127.0.0.1:3080` 或配置 `[server] url`（仅建议 loopback） |
| 自定义日志文件 | `dshtui --log /tmp/dshtui.log`（默认 `~/.local/state/dshtui/dshtui.log`，5MB 轮转） |
| 指定 token 环境变量 | `dshtui --token MY_DSH_TOKEN`（只接受环境变量名） |
| perf 日志路径 | 环境变量 `DSHTUI_PERF_LOG=/tmp/perf.log`，或配置 `[perf] log_path`（默认 `/tmp/dshtui-perf.log`） |

### 5. 升级后契约冒烟（V1 REQ-008）

官方 `dsh web` 升级后（或你升级了 dshtui 二进制），验证兼容性：

```bash
# 离线契约（无需后端，mock HTTP/WS）——快，先跑
cargo test --all-targets

# 若已 clone 源码并配好 DSH_TOKEN + 本机 dsh web，跑真实后端冒烟（V1 REQ-008 新增；
# tests/live_smoke.rs 默认 #[ignore]）
cargo test --test live_smoke -- --ignored

# export JSONL live 锁定 + golden 离线复核（D-65；无 token 时 export-golden-check.sh 仍可跑）
DSH_TOKEN=<token> scripts/live-export-lock.sh --golden
scripts/export-golden-check.sh            # 离线（无 token 也可跑）
```

官方新 alpha 升级信号的自动流程见 [README「升级兼容流程」](../README.md#升级兼容流程)
（`upgrade-signal` workflow 开 24h SLA tracking issue）；无需仓库 secret 的协议表面
冒烟用 `scripts/ci-live-smoke.sh`（下载官方 alpha → 隔离起只读实例 → 自跑
`tests/live_alpha_smoke.rs`）。

schema 破坏性变更排查用 `node scripts/schema-compare.mjs --from-snapshot
schemas/dsh-api-schema-0.1.2-rc.1.json --to <新版 bundle 目录>`（或先 `--mode snapshot`
导出新 golden；V1 REQ-008 新增，见 README「schema-compare」节）。
