# 贡献指南

dshtui 的贡献流程：环境准备 → 功能/修复开发 → 全量测试与门禁 → 中文 commit →
PR 评审 → 合入。评审通过前不直接 push 到受保护分支；PR 目标分支为 `main`。

## 环境

- **Rust**：rustup stable（本仓库 MSRV `1.80`，见 Cargo.toml `rust-version`；开发建议
  使用当前 stable）。验证：`cargo --version`、`rustc --version`。
- **依赖纪律**：依赖清单对齐 Notes/02-architecture.md §5（ratatui/crossterm/tokio/
  reqwest/tokio-tungstenite/nucleo/toml/tracing/thiserror），**不引入计划外框架**
  （Cargo.toml 有注释约束）。新增依赖需在 commit 说明理由。
- **Node.js**（可选）：仅用于 `scripts/schema-compare.mjs`（V1 REQ-008）等辅助脚本。
- **后端**（契约冒烟/手动验证用）：本机官方 `dsh web`（`http://127.0.0.1:3080`）+
  `DSH_TOKEN`。纯离线开发不需要它（mock 协议测试覆盖）。

## 测试

全量测试是合入门禁，功能/修复完成后**必须**先跑全量再声称完成：

```bash
cargo test --all-targets                                   # 全量（离线 mock 可跑）
cargo test --all-targets -- --ignored                      # 含 live_smoke（需 DSH_TOKEN + dsh web）
cargo clippy --all-targets --all-features -- -D warnings   # clippy 零告警
cargo fmt --all -- --check                                 # 格式
```

专项（快速定位，全部离线可跑）：

```bash
cargo test --test api_protocol            # 协议 mock HTTP/WS（envelope/mux/会话/export…）
cargo test --test ui_golden               # ratatui TestBackend golden 渲染
cargo test --test keymap                  # 键位解码
cargo test --test model                   # 窗口化模型/搜索/审批/图片…
cargo test --test monitor_protocol        # monitor agent-server mock（REQ-009）
cargo test --test export_rebuild_proto    # export D-46 重建兜底原型门禁
cargo test --test editor_proto            # :edit 编辑器链
cargo test --test keymap_override_proto   # [keymap] 覆盖
```

测试命名风格（tests/ 下）：`api_protocol` / `ui_golden` / `keymap` / `model` /
`monitor_protocol` / `export_rebuild_proto`；高风险/实验性接缝用 `*_proto.rs`
throwaway prototype 门禁模式（先原型验证、再进正式层，参考 `export_rebuild_proto.rs`
头部注释）。集成测试用内联 `TcpListener` mock server，**不依赖真实后端**。

CI 门禁（fmt/clippy/test + 契约冒烟）见 `.github/workflows/ci.yml`（V1 REQ-008 落地）。

## 代码风格

- **格式**：`cargo fmt` 统一；提交前跑 `cargo fmt --all -- --check`。
- **Lint**：`cargo clippy --all-targets --all-features -- -D warnings` 零告警
  （含 `field_reassign_with_default` 这类 pedantic 项——用 struct update 字面量规避）。
- **语言**：代码注释以中文为主（模块/结构性注释可英文，对齐相邻文件惯例）；
  标识符、类型、终端输出用英文。
- **分层纪律**（架构见 docs/architecture.md）：
  - 协议字段/信封/RPC 封装只进 `api/`，禁止散落硬编码；
  - 纯逻辑（窗口合并、搜索、解码）放 `model/`（不依赖 reqwest/ratatui，纯同步可测）；
  - 状态变更走 `AppState` reducer（单写），UI/transport 不直接持有可变模型；
  - 键位改动进 `input/keymap.rs` 的对应模态 + `default_key_tables()`，并同步帮助文案。
- **注释即文档**：关键设计决策写进文件头部注释（含 REQ/AC/D-编号），代码实现与
  Notes/ADR 口径一致；不要只写实现不留依据。

## Commit 规范

仓库实际风格（`git log` 统计 200 条内：feat 46 / fix 17 / style 2 / refactor 1），
统一为：

```
<type>(dshtui): <中文描述>
```

- `type`：`feat`（功能/验收接线）、`fix`（缺陷/评审修复）、`style`（fmt/clippy 收敛）、
  `refactor`（结构性调整）、`docs`（文档/注释）、`chore`（构建/工具/权限位等杂项）；
  范围恒为 `dshtui`。
- 描述为**中文**，一句话概括 + 破折号展开关键点；涉及需求时标注 `REQ-00X` /
  `AC-XXX` / `D-XX` 编号；行为改动附**测试证据**（如「AC-008-09/10 测试证据」、
  「full NNN PASS」）。

示例（来自仓库历史）：

```
feat(dshtui): REQ-008 Step 1 可观测接线——FrameSampler percentile/p99 + PerfLogEntry
七字段行 + 主 TUI perf 采样（每帧 tick/5s 写日志）+ page/search 耗时采集 +
ws_reconnects reducer 单调计数 + config [perf] log/log_path 与 [log] max_bytes；
AC-008-09/10 测试证据

fix(dshtui): REQ-007 D-50 feedback 端点不可用本地降级——…；负向/恢复路径测试

style(dshtui): REQ-007 fmt/clippy 零告警——…
```

每个 commit 保持单一主题；大改动按 Step/REQ 拆小提交，便于回滚与评审。合并用
PR merge（`Merge pull request #N from ndzuki/task/xxx`），分支命名
`task/0NN-dshtui-vXX-<主题>`。

## 评审流程

1. **自检**：全量测试 + clippy + fmt 通过；运行 `git status` 确认只含本主题改动。
2. **变更自查**：跑变更审查（worktree/同步、secret 扫描、格式、环境一致、回归）——
   确认无回归、无凭据泄漏后再送审。
3. **PR 评审**：code-review 双轴——Standards（是否遵守本仓库代码规范：分层/注释/
   lint）与 Spec（是否符合发起 issue/REQ 的验收口径）；评审人逐项给 PASS/WARN。
4. **评审修复**：评审发现的问题按发现回提交 `fix(dshtui)`，不要 rewrite 已送审历史
   （除非评审明确要求 squash）。
5. **合入**：通过后 merge 到 `main`；发布类改动（版本号/Cargo.toml、.github/release.yml）
   走 V1 REQ-008 收口流程（CI 门禁 + cargo-dist tag）。

## 文档同步义务

改动触及以下面时**必须**同步对应文档（否则 CI/文档门禁会拦）：

- 协议字段/schema：更新 docs/architecture.md 与 mock 契约测试；官方升级走
  `node scripts/schema-compare.mjs --from … --to …`（V1 REQ-008）；
- export JSONL 行格式：格式已锁定，变更必须走 schema 对比 + 契约测试更新；
- CLI 子命令/选项：更新 `src/config.rs` usage_text 与 README；
- 配置项：更新 `config.example.toml`（带注释与默认值）与 README 配置节；
- 键位：更新 `input/keymap.rs` 注释/默认表 + README 键位速查；
- 架构/边界（ADR）：更新 docs/architecture.md。

## 禁止事项

- 不直接 push 到 `main`/受保护分支（走 PR）；
- 不编辑生成文件（`*.pb.go`、`*_mock.go`、`*/gen/*` 等，本仓库类比：不手改 Cargo.lock
  之外的锁文件产物、target/ 产物）；
- 破坏性操作（`rm -rf`、force push、删除分支）需确认；
- 功能开发或 bug 修复后**未跑全量测试**不得声称完成（回归测试强制）。
