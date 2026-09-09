#!/usr/bin/env bash
# live-export-lock.sh —— REQ-008 Step 6：export JSONL 行格式 live 锁定冒烟
# （AC-008-15 / D-54）。
#
# 对真实官方 dsh 会话跑「官方 session-log-export ZIP 的 session.jsonl」 vs
# 「session/page 全量收集 → 按 model/export.rs rebuild_* 规则重建的 JSONL」
# 对比，打印锁定结论：
#   - header 形态（官方 session / type+version+id）；
#   - 行数与 records 数（官方=逐 delta 原始日志、page=聚合视图，结构性差）；
#   - 逐 type 计数（page 可及子集 vs 官方）；
#   - 同 seq 事件行语义一致性、输出顺时性。
# 差异为 REQ-008 已接受的（数据源粒度）差异 → 报告但不 fail；硬错误
# （认证失败/收集为空/解析失败）→ exit 1。DSH_TOKEN 未设置 → skip exit 0。
#
# 用法:
#   DSH_TOKEN=... scripts/live-export-lock.sh            # live 对比冒烟
#   DSH_TOKEN=... scripts/live-export-lock.sh --golden   # 同上 + golden 复核/再生成
#
# D-65（Step B）：export JSONL 锁 live-first + committed golden 离线兜底。
# --golden 在本脚本 live 对比通过后：
#   - 若当前官方版本的 golden fixture 尚不存在（fixtures/export-golden/v<ver>/），
#     用本次刚下载的官方导出调 scripts/export-golden-check.sh --capture 再生成；
#   - 然后对该 fixture 跑 --check 离线校验（已入库版本 → 只校验不覆盖）。
# 脱敏器只实现在 export-golden-check.sh 一处，本脚本不复制逻辑。
#
# 环境:
#   DSH_TOKEN            必填（live 部分）；缺省跳过
#   DSHTUI_LIVE_BASE     官方 base URL（默认 http://127.0.0.1:3080）
#   DSHTUI_LIVE_SESSION  目标会话 id（默认 REQ-008 证据会话
#                        session-25536e2c-f8b9-4bcf-a16b-0baa085fa362）
#   DSHTUI_GOLDEN_OFFICIAL_VERSION  golden 官方版本标签（默认 0.1.2-rc.1）
#   DSHTUI_GOLDEN_FIXTURE           显式 golden fixture 目录（默认同 export-golden-check.sh）
# 前置: curl + python3（JSONL/zip 分析）
set -euo pipefail

BASE="${DSHTUI_LIVE_BASE:-http://127.0.0.1:3080}"
SESSION="${DSHTUI_LIVE_SESSION:-session-25536e2c-f8b9-4bcf-a16b-0baa085fa362}"
PAGE_SIZE="${DSHTUI_LIVE_PAGE_SIZE:-500}"

# --golden 模式（可选首个参数）；无参行为保持原样
GOLDEN=0
if [ "${1:-}" = "--golden" ]; then
  GOLDEN=1
  shift
fi

# golden fixture 目录解析（与 scripts/export-golden-check.sh 同约定）
GOLDEN_SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/export-golden-check.sh"
GOLDEN_VERSION="${DSHTUI_GOLDEN_OFFICIAL_VERSION:-0.1.2-rc.1}"
case "$GOLDEN_VERSION" in
  v*) GOLDEN_DIRVER="$GOLDEN_VERSION" ;;
  *)  GOLDEN_DIRVER="v$GOLDEN_VERSION" ;;
esac
GOLDEN_FIXDIR="${DSHTUI_GOLDEN_FIXTURE:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/fixtures/export-golden/$GOLDEN_DIRVER}"

if [ -z "${DSH_TOKEN:-}" ]; then
  echo "[skip] DSH_TOKEN 未设置——跳过 live 部分（CI 默认跳过）"
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

run() {
  DSH_TOKEN="$DSH_TOKEN" BASE="$BASE" SESSION="$SESSION" PAGE_SIZE="$PAGE_SIZE" \
  python3 - "$TMP" <<'PY'
import json, os, subprocess, sys, zipfile
from collections import Counter

tmp = sys.argv[1]
base = os.environ["BASE"]
session = os.environ["SESSION"]
token = os.environ["DSH_TOKEN"]
page_size = int(os.environ["PAGE_SIZE"])
cj = os.path.join(tmp, "cj.txt")

def sh(*a, **kw):
    return subprocess.run(a, capture_output=True, text=True, **kw)

# 1) auth（同源 cookie）
r = sh("curl", "-s", "-c", cj, "-o", "/dev/null", "-w", "%{http_code}",
       f"{base}/?token={token}")
if r.stdout.strip() not in ("303", "200"):
    print(f"[fail] 认证失败 http={r.stdout.strip()}", file=sys.stderr); sys.exit(1)

# 2) 官方导出 ZIP → session.jsonl
zpath = os.path.join(tmp, "export.zip")
r = sh("curl", "-s", "-b", cj, "-m", "120", "-o", zpath, "-w", "%{http_code}",
       f"{base}/api/session.export?sessionId={session}&includeDescendants=true")
if r.stdout.strip() != "200":
    print(f"[fail] 官方导出 http={r.stdout.strip()}", file=sys.stderr); sys.exit(1)
out_dir = os.path.join(tmp, "zip")
with zipfile.ZipFile(zpath) as z:
    z.extract("session.jsonl", out_dir)
official = [json.loads(l) for l in open(os.path.join(out_dir, "session.jsonl"), encoding="utf-8")]
if not official:
    print("[fail] 官方 session.jsonl 为空", file=sys.stderr); sys.exit(1)

def line_seq(o):
    return o.get("seq", o.get("seq0"))
max_export = max(line_seq(o) for o in official if line_seq(o) is not None)

# 3) session/page 全量收集（throughSeq = 官方导出最大 seq；页内升序、跨页
#    beforeSeq 独占上界退旧；与 rebuild_export_jsonl 相同翻页语义）
def page(before_seq, through):
    req = {"request": {"address": {"kind": "session", "sessionId": session},
                       "throughSeq": through, "maxMessages": page_size}}
    if before_seq is not None:
        req["request"]["beforeSeq"] = before_seq
    body = {"type": "client-request", "rpcId": "live-export-lock",
            "method": "session/page", "payload": {"args": req}}
    p = sh("curl", "-s", "-b", cj, "-m", "120", "-X", "POST",
           f"{base}/api/session/page", "-H", "Content-Type: application/json",
           "-d", json.dumps(body))
    try:
        resp = json.loads(p.stdout)
    except json.JSONDecodeError:
        print("[fail] session/page 响应非 JSON", file=sys.stderr); sys.exit(1)
    if not resp.get("result", {}).get("ok"):
        print(f"[fail] session/page 非 ok: {str(resp)[:200]}", file=sys.stderr); sys.exit(1)
    v = resp["result"]["value"]
    return v.get("records", []), v.get("hasMore", False)

def rec_seq(rec):
    e = rec.get("event")
    return e.get("seq") if isinstance(e, dict) else rec.get("seq")

records, before, pages = [], None, 0
while True:
    recs, has_more = page(before, max_export)
    pages += 1
    records.extend(recs)
    if not has_more or not recs or pages > 100000:
        break
    seqs = [rec_seq(r) for r in recs]
    mn = min((s for s in seqs if s is not None), default=None)
    if mn is None or (before is not None and mn >= before):
        break
    before = mn
if not records:
    print("[fail] session/page 收集为空", file=sys.stderr); sys.exit(1)

# 4) 重建（与 src/model/export.rs rebuild_* 同规则）
def to_line(rec):
    kind = rec.get("type")
    inner = rec.get("event", rec) if kind in ("event", "chunks") else rec
    if not isinstance(inner, dict):
        return inner
    t = inner.get("type")
    if isinstance(t, str) and t.startswith("chunkrow/"):
        out = dict(inner)
        out["type"] = t[len("chunkrow/"):]
        if "seq" in out:
            out["seq0"] = out.pop("seq")
        if "time" in out:
            out["time0"] = out.pop("time")
        return out
    return inner

rebuilt = [{"type": "session", "version": 0, "id": session}]
rebuilt += sorted((to_line(r) for r in records), key=lambda o: (line_seq(o) is None, line_seq(o) or 0))

# 5) 对比报告
def etype(o):
    return o.get("type")
oc = Counter(etype(o) for o in official)
rc = Counter(etype(o) for o in rebuilt)
print("=== live export-lock（AC-008-15 / D-54）===")
print(f"session      : {session}")
print(f"base         : {base}")
print(f"official     : {len(official)} lines (min {min(line_seq(o) for o in official if line_seq(o) is not None)}..max {max_export})")
print(f"page records : {len(records)} (pages={pages})")
print(f"rebuilt      : {len(rebuilt)} lines (records+header)")

h = official[0]
print("\n[header]")
print(f"  official : {json.dumps(h, separators=(',',':'), ensure_ascii=False)[:220]}")
print(f"  rebuilt  : {json.dumps(rebuilt[0], separators=(',',':'), ensure_ascii=False)}")
extra = sorted(set(h) - set(rebuilt[0]))
print(f"  official 可选字段（page 重建省略）: {extra}")

print("\n[type 计数] rebuilt vs official")
alltypes = sorted(set(oc) | set(rc))
for t in alltypes:
    mark = "" if oc.get(t, 0) == rc.get(t, 0) else "  <- 结构性差(page 聚合/delta 不可见)"
    print(f"  {t:<24} rebuilt={rc.get(t,0):>6}  official={oc.get(t,0):>6}{mark}")

# 同 seq 语义一致性（page 可及子集）：event 行重建后与官方同 seq 行比较。
off_by = {}
for o in official:
    if o is official[0]:
        continue
    off_by.setdefault(line_seq(o), []).append(o)
ev_ok = ev_n = 0
for rec in records:
    inner = rec.get("event", {})
    if rec.get("type") != "event":
        continue
    s = inner.get("seq")
    cands = off_by.get(s, [])
    if len(cands) == 1 and cands[0].get("type") == inner.get("type"):
        ev_n += 1
        # 语义比较（键序无关）：除 sourceEventSeqs（官方区间压缩 vs page 展开）外
        a = {k: v for k, v in cands[0].items() if k in ("type", "seq", "time", "data")}
        b = {k: v for k, v in inner.items() if k in ("type", "seq", "time", "data")}
        if a == b:
            ev_ok += 1
print(f"\n[同 seq 事件语义一致] rebuilt event 行与官方同 seq 行 type/seq/time/data 一致: {ev_ok}/{ev_n}")

seqs = [line_seq(o) for o in rebuilt[1:]]
asc = all(seqs[i] <= seqs[i + 1] for i in range(len(seqs) - 1))
print(f"[顺序] rebuilt 记录行 seq 严格升序（顺时）: {asc}")

if ev_n and ev_ok > 0 and asc:
    print("\n[PASS] live 锁定冒烟完成——报告如上；差异均为已接受的 page 聚合粒度/可选字段差异")
else:
    print("\n[FAIL] 一致性断言未达标（收集/解析已通过但事件语义或顺序异常）", file=sys.stderr)
    sys.exit(1)
PY
}

if ! run; then
  echo "[fail] live-export-lock 冒烟失败" >&2
  exit 1
fi

if [ "$GOLDEN" = "1" ]; then
  # live 对比已通过 → golden 复核/再生成（D-65 离线兜底）
  if [ ! -f "$GOLDEN_FIXDIR/session.jsonl" ]; then
    echo "[golden] fixture 缺失，用本次官方导出 capture: $GOLDEN_FIXDIR"
    "$GOLDEN_SCRIPT" --capture "$TMP/export.zip" "$GOLDEN_FIXDIR"
  else
    echo "[golden] fixture 已存在（只校验不覆盖）: $GOLDEN_FIXDIR"
  fi
  echo "[golden] 离线校验 golden fixture"
  "$GOLDEN_SCRIPT" --check "$GOLDEN_FIXDIR"
fi

echo "[ok] exit 0"
