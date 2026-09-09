#!/usr/bin/env bash
# export-golden-check.sh —— REQ-008 Step B（D-65）：export JSONL 行格式 golden 离线回归
# （AC-008-15）。
#
# D-65 语义：export JSONL 锁是「live-first」的——官方路由可达时对真实下载的
# 官方 `/api/session.export` ZIP 做实时字节/结构比对（那是 scripts/live-export-lock.sh
# 的主路径）；同时把一份「已脱敏的官方字节样本」作为 **committed golden fixture**
# （fixtures/export-golden/<official_version>/）离线兜底，使端点不可用/无 DSH_TOKEN
# 时锁状态永不因端点缺失而被误判。D-70 口径：字节级锁范围 = 官方同源 HTTP 主路径
# + export golden（离线回归）；page 重建只是语义子集，不在本脚本锁定范围。
#
# 本脚本 = golden 的唯一事实源：
#   - 离线校验（默认/check）：确定性回归——meta.json 字段、session.jsonl 的
#     sha256/行数/逐行 JSON/header 形态（type=session+version+id）/
#     record 形态（事件行 type+seq/time、`*-chunks` 行 seq0/time0）/
#     seq 非递减顺时序。干净则 PASS exit 0，违规则逐条上 stderr 且 exit 1。
#   - --capture：把官方导出（ZIP 或 session.jsonl）**确定性脱敏**成 golden
#     （内容 tokens → [redacted-N]、时间戳平移固定伪基点；type/键/结构/seq 关系
#     原样保留）并写 meta.json（含 sha256）——脱敏器只在此实现一份。
#   - --live：live-first 的结构复检——需要 DSH_TOKEN + 可达官方路由；对 golden 的
#     source_session 重抓官方导出做同样行格式/形状校验，并与 golden 做
#     「仅结构」byte-diff 报告（内容/行数漂移只报告不判失败；会话会长大）。
#   - --self-test：负向自检——复制 golden 制造「删一行 / 坏一行 / meta 哈希被篡改」
#     三种违规并断言每次都被 exit 1 捕获；全部捕获才 exit 0（CI 用）。
#
# 用法:
#   scripts/export-golden-check.sh                 # 离线校验（默认）
#   scripts/export-golden-check.sh --check [DIR]   # 同上（可指定 fixture 目录）
#   scripts/export-golden-check.sh --capture <zip-or-jsonl> <outdir>
#   scripts/export-golden-check.sh --live [DIR]
#   scripts/export-golden-check.sh --self-test [DIR]
#
# 环境:
#   DSHTUI_GOLDEN_OFFICIAL_VERSION  官方版本（默认 0.1.2-rc.1；决定 fixture 目录名）
#   DSHTUI_GOLDEN_FIXTURE           显式 fixture 根目录（默认 fixtures/export-golden/<version>）
#   DSHTUI_GOLDEN_CAPTURED_AT       --capture 写入 meta.captured_at 的 ISO（默认当前 UTC）
#   DSHTUI_GOLDEN_BASE_MS           脱敏时间基点（默认 1700000000000）
#   DSH_TOKEN / DSHTUI_LIVE_BASE    --live 用（同 live-export-lock.sh）
#   DSHTUI_LIVE_SESSION             --live 目标会话（默认读 golden meta.source_session）
#
# 前置: curl + python3（零第三方依赖，与 scripts/live-export-lock.sh 一致）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# official_version 是 meta 里的版本标签（默认 0.1.2-rc.1）；fixture 目录名带 v
# 前缀（约定 fixtures/export-golden/v<version>/），显式 DSHTUI_GOLDEN_FIXTURE 可覆盖。
VERSION="${DSHTUI_GOLDEN_OFFICIAL_VERSION:-0.1.2-rc.1}"
case "$VERSION" in
  v*) DIRVER="$VERSION" ;;
  *)  DIRVER="v$VERSION" ;;
esac
FIXTURE="${DSHTUI_GOLDEN_FIXTURE:-$ROOT/fixtures/export-golden/$DIRVER}"

usage() {
  cat >&2 <<EOF
用法: scripts/export-golden-check.sh [check|--check [DIR] | --capture SRC OUTDIR |
                                      --live [DIR] | --self-test [DIR] | -h]
  check/--check  离线校验 committed golden（默认；DIR 缺省 $FIXTURE）
  --capture      从官方 ZIP 或 session.jsonl 生成脱敏 golden（确定性）+ meta.json
  --live         重抓 golden source_session 的官方导出做结构复检 + 结构 diff 报告
  --self-test    制造删行/坏行/hash 篡改三种违规，断言全被 exit 1 捕获
EOF
}

# 内嵌 python 主体：按子命令分发（offline/capture/live），校验/脱敏逻辑只此一份。
run_py() {
  DSH_TOKEN="${DSH_TOKEN:-}" \
  DSHTUI_LIVE_BASE="${DSHTUI_LIVE_BASE:-http://127.0.0.1:3080}" \
  DSHTUI_LIVE_SESSION="${DSHTUI_LIVE_SESSION:-}" \
  DSHTUI_GOLDEN_OFFICIAL_VERSION="$VERSION" \
  DSHTUI_GOLDEN_CAPTURED_AT="${DSHTUI_GOLDEN_CAPTURED_AT:-}" \
  DSHTUI_GOLDEN_BASE_MS="${DSHTUI_GOLDEN_BASE_MS:-1700000000000}" \
  python3 - "$@" <<'PY'
import hashlib, json, os, re, shutil, subprocess, sys, tempfile, zipfile

GOLDEN_SESSION_ID = "session-golden-00000000-0000-0000-0000-000000000000"
GOLDEN_CWD = "/home/golden"   # 伪 cwd（非真实用户路径）
GOLDEN_PRESET = "default"     # 伪 agentPreset（真实 preset 名不入库）

# 值是「格式判别符 / 域 token」的键 → 原样保留（使行格式契约可读可回归）。
KEEP_KEYS = {
    "type", "kind", "role", "mode", "preset", "policy", "provider", "model",
    "surfaceOp", "blockType", "api", "stopReason", "reasoningEffort", "plugin",
    "target", "clientTimeZone", "code", "titleProvider", "thinkingSignature",
    "name", "enum", "required", "$schema", "reason", "status",
}
# 值是「真实内容」的键 → 无条件替换为 [redacted-N]。
CONTENT_KEYS = {
    "text", "texts", "content", "message", "arguments", "argumentsDelta",
    "command", "description", "system", "title", "args", "id", "rpcId",
    "callId", "toolCallId", "retryId", "responseId", "cwd", "error", "failure",
}
UUID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
SENS_RE = re.compile(
    r"(?i)call_00_|chatcmpl-|session-[0-9a-fA-F]{8}-|/home/|/root/|/Users/|"
    r"/private/|/var/|api[_-]?key|bearer\s|token[=:]")


def err(msg):
    print(msg, file=sys.stderr)


def content_like(s):
    """非内容键下仍像真实内容的字符串（长/含空白/非 ASCII/敏感形态）。"""
    if len(s) > 60:
        return True
    if re.search(r"\s", s):
        return True
    if not s.isascii():
        return True
    if UUID_RE.match(s):
        return True
    if SENS_RE.search(s):
        return True
    return False


def desensitize_lines(raw_lines, base_ms):
    """确定性脱敏：保行数/逐行 type 序/键结构/嵌套/seq 序；内容 token 与
    时间戳平移。base_ms = 固定伪时间基点（epoch ms）。"""
    header = json.loads(raw_lines[0])
    if not (isinstance(header, dict) and header.get("type") == "session"):
        raise ValueError("first line is not a session header")
    delta = base_ms - header.get("createdAt", base_ms)  # 恒等平移 → 相对间隔不变

    def desensitize_line(line):
        obj = json.loads(line)
        is_header = isinstance(obj, dict) and obj.get("type") == "session" and "id" in obj
        if is_header:
            obj["id"] = GOLDEN_SESSION_ID
            if "createdAt" in obj:
                obj["createdAt"] = base_ms
            if "cwd" in obj:
                obj["cwd"] = GOLDEN_CWD
            if "agentPreset" in obj:
                obj["agentPreset"] = GOLDEN_PRESET
        else:
            for k in ("time", "time0"):
                if isinstance(obj.get(k), (int, float)):
                    obj[k] = obj[k] + delta
        fixed = {"id", "createdAt", "cwd", "agentPreset"} if is_header else set()
        counter = [0]

        def redact():
            counter[0] += 1
            return "[redacted-%d]" % counter[0]

        def walk(v, key):
            if isinstance(v, dict):
                return {k: walk(x, k) for k, x in v.items()}   # 键序/键集原样保留
            if isinstance(v, list):
                return [walk(x, key) for x in v]
            if isinstance(v, str):
                if key in fixed:
                    return v
                if key in CONTENT_KEYS:
                    return redact()
                if key in KEEP_KEYS:
                    return v
                if content_like(v):
                    return redact()
                return v
            return v

        return walk(obj, "")

    out = []
    for ln in raw_lines:
        if not ln.strip():
            raise ValueError("blank line in input")
        out.append(json.dumps(desensitize_line(ln), ensure_ascii=False))
    return out


# ---- 行格式契约校验（offline 与 live 共用）--------------------------------
def validate_lines(lines, label, errors):
    """校验：逐行 JSON；首行 header（type=session+version+id）；record 行形态
    （事件 type+seq/time、`*-chunks` seq0/time0）；seq/seq0 非递减顺时。"""
    objs = []
    for i, ln in enumerate(lines, 1):
        try:
            o = json.loads(ln)
        except Exception as e:
            errors.append("%s: line %d: not valid JSON (%s)" % (label, i, e))
            continue
        if not isinstance(o, dict):
            errors.append("%s: line %d: not a JSON object" % (label, i))
            continue
        objs.append(o)
    if not objs:
        errors.append("%s: empty (no parseable lines)" % label)
        return objs
    h = objs[0]
    if h.get("type") != "session":
        errors.append("%s: line 1 is not a session header (type=%r)" % (label, h.get("type")))
    for k in ("version", "id"):
        if k not in h:
            errors.append("%s: line 1 header missing field %r" % (label, k))
    prev = None
    for i, o in enumerate(objs[1:], 2):
        t = o.get("type")
        if not isinstance(t, str) or not t:
            errors.append("%s: line %d: missing/invalid type" % (label, i))
            continue
        if t.endswith("-chunks"):
            for k in ("seq0", "time0"):
                if not isinstance(o.get(k), int):
                    errors.append("%s: line %d (%s): field %s missing/non-int" % (label, i, t, k))
            cur = o.get("seq0")
        else:
            for k in ("seq", "time"):
                if not isinstance(o.get(k), int):
                    errors.append("%s: line %d (%s): field %s missing/non-int" % (label, i, t, k))
            cur = o.get("seq")
        if not isinstance(cur, int):
            continue
        if prev is not None and cur < prev:
            errors.append("%s: line %d (%s): seq %s < previous %s (not chronological)"
                          % (label, i, t, cur, prev))
        prev = cur
    return objs


def validate_fixture(fixture):
    """离线校验 committed golden；返回 (meta, errors)。"""
    errors = []
    mp = os.path.join(fixture, "meta.json")
    sp = os.path.join(fixture, "session.jsonl")
    if not (os.path.isfile(mp) and os.path.isfile(sp)):
        errors.append("golden fixture missing: %s/{meta.json,session.jsonl}" % fixture)
        return None, errors
    try:
        meta = json.load(open(mp, encoding="utf-8"))
    except Exception as e:
        errors.append("meta.json not valid JSON: %s" % e)
        return None, errors
    if not isinstance(meta, dict):
        errors.append("meta.json is not a JSON object")
        return None, errors
    required = [("schema_version", int), ("official_version", str),
                ("captured_at", str), ("source_session", str),
                ("desensitized", bool), ("lines", int), ("sha256", str)]
    for k, t in required:
        if k not in meta:
            errors.append("meta.json missing required field %r" % k)
        elif not isinstance(meta[k], t):
            errors.append("meta.json field %r has wrong type (want %s)"
                          % (k, t.__name__))
    if not isinstance(meta.get("sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", meta["sha256"]):
        errors.append("meta.json sha256 not a 64-hex string")
    if meta.get("desensitized") is not True:
        errors.append("meta.json desensitized != true")
    try:
        raw = open(sp, "rb").read()
    except OSError as e:
        errors.append("session.jsonl unreadable: %s" % e)
        return meta, errors
    if meta.get("sha256") and hashlib.sha256(raw).hexdigest() != meta["sha256"]:
        errors.append("session.jsonl sha256 mismatch vs meta.json")
    try:
        lines = raw.decode("utf-8").splitlines()
    except UnicodeDecodeError as e:
        errors.append("session.jsonl not UTF-8: %s" % e)
        return meta, errors
    if meta.get("lines") is not None and len(lines) != meta["lines"]:
        errors.append("line count %d != meta.lines %d" % (len(lines), meta["lines"]))
    if errors:
        return meta, errors
    validate_lines(lines, "session.jsonl", errors)
    return meta, errors


def cmd_offline(fixture):
    meta, errors = validate_fixture(fixture)
    if errors:
        for e in errors:
            err("[fail] " + e)
        return 1
    print("[PASS] export-golden offline check (%s): meta ok, session.jsonl %d lines "
          "sha256 %s — header/record shape/chronological order ok"
          % (fixture, meta["lines"], meta["sha256"]))
    return 0


def cmd_capture(src, outdir):
    if src.endswith(".zip"):
        try:
            with zipfile.ZipFile(src) as z:
                data = z.read("session.jsonl").decode("utf-8")
        except Exception as e:
            err("[fail] --capture: cannot read %s: %s" % (src, e))
            return 1
    else:
        try:
            with open(src, encoding="utf-8") as f:
                data = f.read()
        except OSError as e:
            err("[fail] --capture: cannot read %s: %s" % (src, e))
            return 1
    raw_lines = data.splitlines()
    if not raw_lines:
        err("[fail] --capture: source empty")
        return 1
    errs = []
    validate_lines(raw_lines, "source", errs)  # 官方源先过同一行格式契约
    if errs:
        for e in errs:
            err("[fail] --capture: source violates contract: " + e)
        return 1
    header = json.loads(raw_lines[0])
    source_session = (os.environ.get("DSHTUI_LIVE_SESSION")
                      or header.get("id") or "unknown")
    base_ms = int(os.environ["DSHTUI_GOLDEN_BASE_MS"])
    try:
        out = desensitize_lines(raw_lines, base_ms)
    except ValueError as e:
        err("[fail] --capture: %s" % e)
        return 1
    os.makedirs(outdir, exist_ok=True)
    sp = os.path.join(outdir, "session.jsonl")
    with open(sp, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")
    sha = hashlib.sha256(open(sp, "rb").read()).hexdigest()
    captured_at = os.environ.get("DSHTUI_GOLDEN_CAPTURED_AT") or ""
    if not captured_at:
        import datetime
        captured_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    meta = {
        "schema_version": 1,
        "official_version": os.environ["DSHTUI_GOLDEN_OFFICIAL_VERSION"],
        "captured_at": captured_at,
        "source_session": source_session,
        "desensitized": True,
        "lines": len(out),
        "sha256": sha,
        "notes": "官方同源 export 的确定性脱敏样本（D-65 golden）：内容 tokens "
                 "（路径/消息文本/工具参数与描述/系统提示/标题/call·uuid·response "
                 "id 等）替换为 [redacted-N]，时间戳整体平移到固定伪基点，保留 "
                 "逐行 type/键结构/seq·time 序关系；仅作离线行格式/顺序回归基线，"
                 "绝不入库真实内容。",
    }
    with open(os.path.join(outdir, "meta.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, ensure_ascii=False, indent=2)
        f.write("\n")
    print("[PASS] export-golden capture: %s -> %s (%d lines, sha256 %s)"
          % (src, outdir, len(out), sha))
    return 0


def shape(o):
    return (o.get("type"), tuple(sorted(o.keys())))


def cmd_live(fixture):
    # 0) golden 本身先过离线校验（golden 坏则 live 报告无意义）
    meta, errors = validate_fixture(fixture)
    if errors:
        for e in errors:
            err("[fail] --live: golden invalid: " + e)
        return 1
    if not os.environ.get("DSH_TOKEN"):
        print("[skip] DSH_TOKEN 未设置——跳过 --live（离线 golden 校验不受影响）")
        return 0
    base = os.environ["DSHTUI_LIVE_BASE"]
    session = (os.environ.get("DSHTUI_LIVE_SESSION") or meta.get("source_session") or "")
    if not session:
        err("[fail] --live: no session (set DSHTUI_LIVE_SESSION or meta.source_session)")
        return 1
    tmp = tempfile.mkdtemp(prefix="export-golden-live-")
    try:
        cj = os.path.join(tmp, "cj.txt")
        r = subprocess.run(["curl", "-s", "-c", cj, "-o", "/dev/null", "-w", "%{http_code}",
                            "%s/?token=%s" % (base, os.environ["DSH_TOKEN"])],
                           capture_output=True, text=True)
        if r.stdout.strip() not in ("303", "200"):
            err("[fail] --live: 认证失败 http=%s" % r.stdout.strip())
            return 1
        zp = os.path.join(tmp, "export.zip")
        r = subprocess.run(["curl", "-s", "-b", cj, "-m", "120", "-o", zp,
                            "-w", "%{http_code}",
                            "%s/api/session.export?sessionId=%s&includeDescendants=true"
                            % (base, session)],
                           capture_output=True, text=True)
        code = r.stdout.strip()
        if code != "200":
            # 认证已过而导出非 200 → 会话多半已不存在：graceful skip，不误判锁
            print("[skip] --live: session %s 官方导出不可用（http=%s）——"
                  "graceful skip，离线 golden 保持有效" % (session, code))
            return 0
        outdir = os.path.join(tmp, "zip")
        with zipfile.ZipFile(zp) as z:
            z.extract("session.jsonl", outdir)
        live_lines = open(os.path.join(outdir, "session.jsonl"),
                          encoding="utf-8").read().splitlines()
        if not live_lines:
            print("[skip] --live: session %s 官方 session.jsonl 为空——graceful skip" % session)
            return 0
        lerrs = []
        validate_lines(live_lines, "live", lerrs)
        if lerrs:
            for e in lerrs:
                err("[fail] --live: " + e)
            return 1
        # 结构 diff 报告（仅报告：会话会长大/内容漂移不判失败）
        g_lines = open(os.path.join(fixture, "session.jsonl"),
                       encoding="utf-8").read().splitlines()
        go = [json.loads(l) for l in g_lines]
        lo = [json.loads(l) for l in live_lines]
        gs = [shape(o) for o in go]
        ls = [shape(o) for o in lo]
        prefix = 0
        for a, b in zip(gs, ls):
            if a == b:
                prefix += 1
            else:
                break
        print("=== export-golden --live 结构复检（D-65 live-first）===")
        print("session      : %s" % session)
        print("base         : %s" % base)
        print("golden       : %d lines (fixture %s)" % (len(gs), fixture))
        print("live         : %d lines" % len(ls))
        print("shape 前缀一致 : %d（前 %d 行逐行 type/键结构相同）" % (prefix, prefix))
        print("live-only shape: %s" % sorted(set(ls) - set(gs)))
        print("golden-only shape: %s" % sorted(set(gs) - set(ls)))
        print("[PASS] --live 结构复检通过——行格式/顺序契约一致；内容与行数漂移仅报告")
        return 0
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    mode = sys.argv[1]
    if mode == "offline":
        return cmd_offline(sys.argv[2])
    if mode == "capture":
        return cmd_capture(sys.argv[2], sys.argv[3])
    if mode == "live":
        return cmd_live(sys.argv[2])
    err("unknown python subcommand: %s" % mode)
    return 2


if __name__ == "__main__":
    sys.exit(main())
PY
}

# ---- 负向自检：复制 golden，制造三种违规，断言每次都被 exit 1 捕获 -------------
selftest() {
  local src="${2:-$FIXTURE}"
  local tmp pass=0 total=3 rc=0
  if [ ! -f "$src/meta.json" ] || [ ! -f "$src/session.jsonl" ]; then
    echo "[fail] --self-test: golden fixture 不存在: $src" >&2
    return 1
  fi
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  run_case() {  # $1=name $2=desc $3=dir —— 断言 offline 必须 exit 非 0
    if run_py offline "$3" >/dev/null 2>&1; then
      echo "[fail] --self-test case $1（$2）未被捕获（offline 返回 0）" >&2
      rc=1
    else
      echo "[ok]   --self-test case $1（$2）被捕获（exit 非 0）"
      pass=$((pass + 1))
    fi
  }

  # 1) 删掉一行（行数/哈希失配）
  mkdir -p "$tmp/c1"
  cp "$src/meta.json" "$src/session.jsonl" "$tmp/c1/"
  head -n -1 "$src/session.jsonl" > "$tmp/c1/session.jsonl"
  run_case c1 "删一行" "$tmp/c1"

  # 2) 中间行损坏（非法 JSON）
  mkdir -p "$tmp/c2"
  cp "$src/meta.json" "$src/session.jsonl" "$tmp/c2/"
  python3 - "$tmp/c2/session.jsonl" <<'MUTPY'
import sys
p = sys.argv[1]
ls = open(p, encoding="utf-8").read().splitlines()
ls[len(ls) // 2] = '{"type":"corrupted","seq":0,'  # 截断 → 非法 JSON
open(p, "w", encoding="utf-8").write("\n".join(ls) + "\n")
MUTPY
  run_case c2 "坏一行" "$tmp/c2"

  # 3) meta.sha256 被篡改
  mkdir -p "$tmp/c3"
  cp "$src/meta.json" "$src/session.jsonl" "$tmp/c3/"
  python3 - "$tmp/c3/meta.json" <<'MUTPY'
import json, sys
p = sys.argv[1]
m = json.load(open(p, encoding="utf-8"))
m["sha256"] = "0" * 64
open(p, "w", encoding="utf-8").write(json.dumps(m, ensure_ascii=False, indent=2) + "\n")
MUTPY
  run_case c3 "hash 篡改" "$tmp/c3"

  if [ "$rc" -eq 0 ] && [ "$pass" -eq "$total" ]; then
    echo "[PASS] --self-test $total/$total 负向用例全部被捕获（fixture $src）"
    return 0
  fi
  echo "[fail] --self-test 未全捕获（$pass/$total）" >&2
  return 1
}

MODE="${1:-check}"
case "$MODE" in
  check|--check)
    run_py offline "${2:-$FIXTURE}"
    ;;
  --capture)
    if [ $# -lt 3 ]; then
      echo "[fail] --capture 需要 <zip-or-jsonl> <outdir>" >&2
      usage
      exit 2
    fi
    run_py capture "$2" "$3"
    ;;
  --live)
    run_py live "${2:-$FIXTURE}"
    ;;
  --self-test)
    selftest "${2:-$FIXTURE}"
    ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    echo "[fail] 未知模式: $MODE" >&2
    usage
    exit 2
    ;;
esac
