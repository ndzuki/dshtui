#!/usr/bin/env bash
# ci-live-smoke.sh —— REQ-008 Step F（D-62）：官方 dsh alpha 一次性只读实例
# 契约表面冒烟。
#
# 背景（D-62）：契约冒烟端点默认本机 3080；CI 没有用户 dsh。本脚本在作业内
# 下载官方目标 dsh 版本（npm 包 @deepseek-ai/dsh，默认 npm dist-tag `alpha`，
# 可用 DSHTUI_ALPHA_VERSION 覆盖为精确版本）→ 用**隔离 DSH_HOME** 起一次性
# web 只读实例（端口 0 = OS 分配）→ 指向该实例跑 `tests/live_alpha_smoke.rs`
# （数据无关协议表面：认证/list envelope/modelCatalog/错误信封/export 路由）
# → 结束 kill + 清理临时目录。实例自身打印 launch token，由脚本解析注入，
# 无需仓库 secret。
#
# 实证（2026-09-09 Gate F prototype）：官方 alpha（0.1.5-alpha.1）在隔离
# DSH_HOME 下可 headless 启动（--no-open + 回环端口），auth fence 返回 401、
# launch token 打印于 stdout；session/list 空实例 items=[]、modelCatalog ok、
# 不存在会话 page 返回 typed error 信封、export 404（路由存在）。零会话实例
# 不覆盖依赖真实数据的行为（那由本机 3080 的 live_smoke.rs 承担）。
#
# 用法:
#   scripts/ci-live-smoke.sh            # 全流程（下载→起→测→清）
# 环境:
#   DSHTUI_ALPHA_VERSION   官方 dsh 版本（默认 npm dist-tag `alpha` 解析）；
#                          例 0.1.5-alpha.1。CI 也可传 `rc`/精确版本。
#   DSHTUI_CI_WORKDIR      工作目录（默认 mktemp -d；结束后删除）
#   DSHTUI_ALPHA_NPM_CACHE npm 缓存目录（默认 $DSHTUI_CI_WORKDIR/.npm-cache）
#   DSHTUI_LIVE_ALPHA_ONLY 设 1 时跳过本地 3080 live_smoke（纯 alpha 模式）
#   NO_CLEANUP             设 1 时保留工作目录（排障）
# 前置: node + npm + curl + cargo（测试在仓库根执行）
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${DSHTUI_ALPHA_VERSION:-alpha}"
WORK="${DSHTUI_CI_WORKDIR:-$(mktemp -d "${TMPDIR:-/tmp}/dshtui-alpha.XXXXXX")}"
CACHE="${DSHTUI_ALPHA_NPM_CACHE:-$WORK/.npm-cache}"

cleanup() {
  if [ "${NO_CLEANUP:-0}" = "1" ]; then
    echo "[ci-live-smoke] NO_CLEANUP=1，保留工作目录: $WORK"
    return
  fi
  # 杀掉本脚本启动的 dsh 实例（如果有）
  if [ -n "${DSH_PID:-}" ]; then
    kill "$DSH_PID" 2>/dev/null || true
    wait "$DSH_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$WORK/profiles/web"
echo '[]' > "$WORK/profiles/web/cordis.yml"
echo '[]' > "$WORK/profiles/web/cordis.patch.yml"
cat > "$WORK/profiles/web/package.json" <<'EOF'
{
  "name": "dsh-profile-web",
  "private": true,
  "dependencies": {},
  "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"], "patchReload": "live" } }
}
EOF

echo "== 1. 解析官方 dsh 版本（dist-tag: $VERSION）=="
RESOLVED="$(npm view "@deepseek-ai/dsh@$VERSION" version 2>/dev/null | tail -1)"
if [ -z "$RESOLVED" ]; then
  echo "[fail] 无法解析 @deepseek-ai/dsh@$VERSION（网络或版本不存在）" >&2
  exit 1
fi
echo "   resolved: $RESOLVED"

echo "== 2. 安装到隔离前缀（npm install，临时缓存）=="
mkdir -p "$WORK/npm-prefix" "$CACHE"
cat > "$WORK/npm-prefix/package.json" <<'EOF'
{ "name": "alpha-install-root", "private": true, "dependencies": {} }
EOF
# 某些 dsh 原生依赖（koffi/node-pty 等）install script 在沙箱/CI 可能被 npm
# 默认拦截——安装本身仍成功；web profile 不需要这些原生子进程能力，忽略告警。
npm install --prefix "$WORK/npm-prefix" "@deepseek-ai/dsh@$RESOLVED" \
  --cache "$CACHE" --no-audit --no-fund --loglevel=error >/dev/null 2>&1 \
  || { echo "[fail] npm install @deepseek-ai/dsh@$RESOLVED 失败" >&2; exit 1; }
DSH_BIN="$WORK/npm-prefix/node_modules/.bin/dsh"
if [ ! -x "$DSH_BIN" ]; then
  echo "[fail] 隔离安装未产出 dsh 可执行文件: $DSH_BIN" >&2
  exit 1
fi
VER="$("$DSH_BIN" --version 2>&1 | tr -d '[:space:]')"
echo "   安装版本: $VER"

echo "== 3. 起一次性只读 web 实例（隔离 DSH_HOME + OS 分配端口）=="
LOG="$WORK/web.log"
PORT_FILE="$WORK/port.txt"
DSH_HOME="$WORK" "$DSH_BIN" --profile web --no-open --host 127.0.0.1 --port 0 \
  > "$LOG" 2>&1 &
DSH_PID=$!

# 等待 stdout 出现 launch token（dsh web 打印 `http://127.0.0.1:<port>/?token=…`）
TOKEN=""
PORT=""
for _ in $(seq 1 60); do
  if grep -q 'token=' "$LOG" 2>/dev/null; then
    URL="$(grep -oE 'http://127\.0\.0\.1:[0-9]+/\?token=[A-Za-z0-9._~-]+' "$LOG" | head -1 || true)"
    if [ -n "$URL" ]; then
      PORT="$(printf '%s' "$URL" | sed -E 's#http://127\.0\.0\.1:([0-9]+)/.*#\1#')"
      TOKEN="$(printf '%s' "$URL" | sed -E 's#.*token=##')"
      break
    fi
  fi
  if ! kill -0 "$DSH_PID" 2>/dev/null; then
    echo "[fail] dsh 实例提前退出；日志尾部:" >&2
    tail -15 "$LOG" >&2 || true
    exit 1
  fi
  sleep 1
done
if [ -z "$TOKEN" ] || [ -z "$PORT" ]; then
  echo "[fail] 未能从实例日志解析 launch token/端口；日志尾部:" >&2
  tail -20 "$LOG" >&2 || true
  exit 1
fi
echo "   实例就绪: http://127.0.0.1:$PORT/  token=${TOKEN:0:6}…"

echo "== 4. 指向实例跑 alpha 契约表面冒烟（tests/live_alpha_smoke.rs）=="
cd "$REPO_ROOT"
DSH_TOKEN="$TOKEN" DSHTUI_LIVE_BASE="http://127.0.0.1:$PORT" \
  cargo test --test live_alpha_smoke -- --ignored --nocapture

echo "== 5. 清理（trap）=="
echo "[ok] ci-live-smoke exit 0"
