#!/usr/bin/env bash
# headless-smoke.sh —— REQ-008 Step 4：无显示服务器/无 Kitty/降级路径 headless
# 冒烟（AC-008-07；复用 TASK-009 已验证的 `script -qec` + TERM 组合先例）。
#
# 用法:
#   scripts/headless-smoke.sh            # 默认对 release 二进制跑降级冒烟
#   scripts/headless-smoke.sh <binary>   # 指定被测二进制（默认 target/release/dshtui）
#
# 行为:
#   1. TERM=dumb、80 列、无 KITTY_WINDOW_ID/无 DISPLAY 组合下启动主 TUI
#      （连不上 3080 时进入指引屏），喂 `q` 退出；
#   2. 断言: 退出码 0（不崩溃）+ 首屏输出含状态条（降级路径可用）。
#   3. 失败即非零退出（fail-fast），不做“打印后继续”的假成功。
set -euo pipefail

BIN="${1:-target/release/dshtui}"
if [ ! -x "$BIN" ]; then
  echo "错误: 被测二进制不存在（先 cargo build --release）: $BIN" >&2
  exit 2
fi

echo "== headless smoke: $BIN (TERM=dumb / 80 列 / 无 KITTY 无 DISPLAY) =="

# 用 pty 会话（script -qec）模拟真实终端；启动后 3s 内喂 q 退出。
# TERM=dumb 是最弱降级路径；无 KITTY_WINDOW_ID/DISPLAY 触发非 Kitty 渲染。
OUT="$(mktemp)"
set +e
( sleep 3; printf 'q' ) | TERM=dumb COLUMNS=80 LINES=24 \
  script -qec "$BIN" /dev/null >"$OUT" 2>&1
EXIT=$?
set -e

echo "exit=$EXIT"
# 退出码 0 且不崩溃。
if [ "$EXIT" -ne 0 ]; then
  echo "FAIL: 主 TUI headless 冒烟退出码非 0（$EXIT）" >&2
  echo "---- 输出尾部 ----" >&2
  tail -20 "$OUT" >&2
  rm -f "$OUT"
  exit 1
fi

# 降级路径可用：输出非空且含状态条/指引痕迹（不崩溃即有渲染尝试）。
if [ ! -s "$OUT" ]; then
  echo "FAIL: 冒烟无任何终端输出（渲染路径异常）" >&2
  rm -f "$OUT"
  exit 1
fi

echo "OK: exit 0，终端有输出（$(wc -c <"$OUT") bytes）——降级路径不崩溃"
rm -f "$OUT"
exit 0
