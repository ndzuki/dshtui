//! Export state (REQ-007 FR-007-04; pure model).
//!
//! Primary path = official same-origin HTTP `/api/session.export` ZIP download
//! (byte-identical, D-41). Fallback (D-46 HARD contract): when the official
//! route is unavailable/fails (404/5xx/transport), rebuild a JSONL export from
//! `session/page` full-history records. 401/403 (permission) NEVER falls back
//! and is never auto-retried. One export per session is single-flight
//! (requestId idempotent).
//!
//! # REQ-008 live 锁定结论（AC-008-15, D-54；2026-09-08 对官方 0.1.2-rc.1
//! session-25536e2c-f8b9-4bcf-a16b-0baa085fa362 实测，证据 scripts/
//! live-export-lock.sh + scratch 报告）
//!
//! 官方 `session.jsonl`（ZIP 内）与 `session/page` 是同一会话的两种粒度视图：
//! 官方行 = 原始逐 delta 日志（每 delta 一条 `assistant/chunk` + 周期性
//! `*-chunks` flush 段，8527 行/样本），page = 聚合历史视图（事件 + 每
//! (turn,step,index) 一条 packed chunkrow，1269 records/样本）。page 数据无法
//! 还原官方逐 delta 行，故「锁定」的是 page 可及子集的 **逐行格式契约**：
//!
//! 1. **header**：`{"type":"session","version":0,"id":<sessionId>}`（官方
//!    首行同形态）。createdAt/cwd/delegationDepth/agentPreset 为官方可选字段，
//!    host 导出时由会话元数据填充；page 重建无可靠来源 → 省略（同官方
//!    dsh-session-persistence-jsonl toHeaderLine 规则：可选字段缺省省略，
//!    不造假值）。移除旧自造 `source:"page-rebuild"` 标记。
//! 2. **解包**：page record `{"type":"event"|"chunks","event":X}` → 输出 X
//!    本身（type/seq/time/data 平铺顶层）。实证：非 chunk 事件 + meta
//!    assistant/chunk（block-start/end/usage/finish）与官方同 seq 行 JSON
//!    语义一致；assistant/message 的 sourceEventSeqs 官方为压缩区间对
//!    `[[a,b]]`、page 为展开列表（语义等价，字节不同）。
//! 3. **chunk 行**：`chunkrow/*-chunks` → 官方 `*-chunks`（reasoning-chunks/
//!    text-chunks/tool-call-chunks），`seq`/`time` → `seq0`/`time0`（官方
//!    flush 行字段名；实证同 seq/time 同位）。data 保持 page 聚合粒度（官方
//!    为逐 flush 段）——结构性已接受差异。
//! 4. **顺序**：官方顺时（seq 升序）。page 响应页内升序、跨页（beforeSeq 走
//!    旧）整体逆序 → 收集全量后按 seq 升序稳定排序输出（见
//!    `rebuild_sort_chronological`）。
//!
//! 行内容用 serde_json 规范 compact 序列化（键按字典序）→ TUI 重建自身字节
//! 稳定、可逐行回读；与官方逐字节差异限于键序与上述数据源粒度，JSON 语义
//! 一致。可测口径 = records 数 + header 行数对账、逐行 schema 回读、
//! 关键事件（type/seq/time/data）与官方同 seq 行一致。

/// Export phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportPhase {
    #[default]
    Idle,
    /// Confirm-path sub-stage (write to a user-chosen path).
    PickingPath,
    Downloading,
    /// D-46: official route unavailable → rebuilding JSONL from session/page
    /// full-history records.
    Rebuilding,
    Done,
    Failed,
}

/// Export state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportState {
    pub visible: bool,
    pub phase: ExportPhase,
    pub session_id: Option<String>,
    /// User-chosen target path (editable buffer during PickingPath).
    pub path: String,
    pub bytes_streamed: u64,
    /// D-46 rebuild progress: 已收集 records 数。
    pub records_collected: u64,
    pub cancelled: bool,
    pub last_error_code: Option<String>,
}

impl ExportState {
    pub fn open(&mut self, session_id: &str, default_path: &str) {
        self.visible = true;
        self.session_id = Some(session_id.to_string());
        self.path = default_path.to_string();
        self.phase = ExportPhase::PickingPath;
        self.bytes_streamed = 0;
        self.records_collected = 0;
        self.cancelled = false;
        self.last_error_code = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    pub fn begin_download(&mut self) -> bool {
        let has_session = self.session_id.as_deref().is_some_and(|s| !s.is_empty());
        if !has_session || self.path.trim().is_empty() {
            return false;
        }
        if self.phase == ExportPhase::Downloading {
            return false; // 在途单飞
        }
        self.phase = ExportPhase::Downloading;
        self.bytes_streamed = 0;
        self.cancelled = false;
        self.last_error_code = None;
        true
    }

    /// D-46：官方路由不可用（404/5xx/transport）→ 转 page 重建兜底。
    /// 允许从 Downloading/Failed（一次路由失败后）进入；在途 Rebuilding 拒绝
    /// 重复（单飞）。
    pub fn begin_rebuild(&mut self) -> bool {
        let has_session = self.session_id.as_deref().is_some_and(|s| !s.is_empty());
        if !has_session || self.path.trim().is_empty() {
            return false;
        }
        if self.phase == ExportPhase::Rebuilding {
            return false; // 在途单飞
        }
        self.phase = ExportPhase::Rebuilding;
        self.records_collected = 0;
        self.cancelled = false;
        self.last_error_code = None;
        true
    }

    pub fn mark_progress(&mut self, bytes: u64) {
        self.bytes_streamed = bytes;
    }

    /// Rebuild 进度：已收集 records 数（单调；cancel/断流后保持最后一次）。
    pub fn mark_rebuild_progress(&mut self, records: u64) {
        self.records_collected = records;
    }

    pub fn finish(&mut self) {
        self.phase = ExportPhase::Done;
    }

    pub fn fail(&mut self, code: String) {
        self.phase = ExportPhase::Failed;
        self.last_error_code = Some(code);
    }
}

/// D-46 page 重建 JSONL header 行（会话 meta）。
///
/// REQ-008 live 锁定（AC-008-15）：与官方 `session.jsonl` 首行同形态——
/// `{"type":"session","version":0,"id":<sessionId>}`。官方可选字段 createdAt/
/// cwd/delegationDepth/agentPreset 由 host 会话元数据填充（toHeaderLine 同
/// 规则：可选字段来源不可得时省略）；page 重建无这些元数据的可靠来源 →
/// 省略（不造假值）。不再带自造 `source` 标记。
pub fn rebuild_header_line(session_id: &str) -> String {
    let header = serde_json::json!({
        "type": "session",
        "version": 0,
        "id": session_id,
    });
    let mut out = serde_json::to_string(&header).unwrap_or_else(|_| "{}".into());
    out.push('\n');
    out
}

/// D-46 page 重建 JSONL 单条 record 行（compact；损坏值以 `{}` 行兜底）。
///
/// REQ-008 live 锁定（AC-008-15）行格式映射：
///   - `{type:"event", event:X}` → 输出 X（type/seq/time/data 平铺顶层）；
///   - `{type:"chunks", event:X}` → 输出 X 并映射为官方 `*-chunks` 行：
///     type 去 `chunkrow/` 前缀、`seq`/`time` → `seq0`/`time0`；
///   - 已是平铺形态（type/seq/time/data 在顶层，含官方 `*-chunks` 形态）的
///     record 原样输出。
pub fn rebuild_record_line(rec: &serde_json::Value) -> String {
    let line = rebuild_record_to_line(rec);
    let mut out = serde_json::to_string(&line).unwrap_or_else(|_| "{}".into());
    out.push('\n');
    out
}

/// 纯映射（不序列化，便于单测逐字段断言）：见 `rebuild_record_line` 说明。
fn rebuild_record_to_line(rec: &serde_json::Value) -> serde_json::Value {
    // 1) 解包 page wrapper：`{type:"event"|"chunks", event:X}` → X。
    let wrapped = matches!(
        rec.get("type").and_then(|t| t.as_str()),
        Some("event" | "chunks")
    );
    let inner = if wrapped {
        rec.get("event").cloned().unwrap_or_else(|| rec.clone())
    } else {
        rec.clone()
    };
    // 2) chunkrow 行映射为官方 `*-chunks` 行（仅当 type 带 chunkrow/ 前缀）。
    let Some(inner_type) = inner.get("type").and_then(|t| t.as_str()) else {
        return inner;
    };
    let Some(stripped) = inner_type.strip_prefix("chunkrow/") else {
        return inner;
    };
    let Some(obj) = inner.as_object() else {
        return inner;
    };
    let mut out = obj.clone();
    out.insert("type".into(), serde_json::json!(stripped));
    // seq/time → seq0/time0（官方 flush 行字段名；其余字段含 data 原样保留）。
    if let Some(seq) = out.remove("seq") {
        out.insert("seq0".into(), seq);
    }
    if let Some(time) = out.remove("time") {
        out.insert("time0".into(), time);
    }
    serde_json::Value::Object(out)
}

/// 从一条 record 的原始 JSON 提取 seq：Event/Chunks 形态
/// `{type:"event"|"chunks",event:{seq}}` 与宽容 `{seq}` 兜底；无 seq 返回
/// None（理论上 page 恒带 seq，chunkrow 的 seq 也在 event 内）。
fn rebuild_record_seq(rec: &serde_json::Value) -> Option<u64> {
    let seq = rec
        .get("event")
        .and_then(|e| e.get("seq"))
        .or_else(|| rec.get("seq"))
        .and_then(|v| v.as_u64());
    seq
}

/// 按 record seq 升序稳定排序（顺时；官方 `session.jsonl` 为 seq 升序时间序）。
///
/// REQ-008 live 实证：官方 0.1.2-rc.1 `session/page` 响应**页内升序**、跨页
/// （beforeSeq 独占上界单调后退取更旧页）整体非单调——收集顺序不可作为输出
/// 顺序。统一在收集全量后调用本函数，保证任何服务器分页顺序下输出均为顺时。
/// 无 seq 的 record（占位/异常）按 None（排最前）处理，稳定排序保持相对序。
pub fn rebuild_sort_chronological(records: &mut [serde_json::Value]) {
    records.sort_by_key(rebuild_record_seq);
}

/// D-46 page 重建 JSONL 纯函数（headless seam）：`records`（fixture/收集的
/// 原始 record JSON，顺序任意）→ 输出 JSONL 文本——header 行 + 每行一条
/// record，先按 seq 顺时排序（见 `rebuild_sort_chronological`）。行格式 =
/// REQ-008 live 锁定结论（模块头）；可测口径 = 行数 = records 数 + header、
/// 逐行可回读、每行与输入经解包/映射后对账。
pub fn rebuild_jsonl(session_id: &str, records: &[serde_json::Value]) -> String {
    let mut out = rebuild_header_line(session_id);
    let mut sorted: Vec<&serde_json::Value> = records.iter().collect();
    sorted.sort_by_key(|r| rebuild_record_seq(r));
    for rec in sorted {
        out.push_str(&rebuild_record_line(rec));
    }
    out
}

/// D-46：从 `session/page` 原始 record JSON 流中推进分页游标。语义对齐官方
/// host `paginate`（0.1.2-rc.1 实读）：`beforeSeq` = 独占上界（返回 seq <
/// beforeSeq 的旧页）；首页 `before_seq=None` 返回最新页。推进 = 以当前页最
/// 旧 record seq 为下一次 beforeSeq，直至 `has_more=false` 或空页/无 seq 页/
/// 达页数硬上界（防死循环）。
///
/// 返回 (next_before_seq, should_stop)。REQ-008 live 锁定（0.1.2-rc.1 实读）：
/// `beforeSeq` 独占上界、页内 seq 升序、跨页单调退旧——本函数只依赖页内
/// min seq 推进，与页序无关。
pub fn rebuild_next_cursor(
    before_seq: Option<u64>,
    records: &[serde_json::Value],
    has_more: bool,
    pages_seen: u64,
    max_pages: u64,
) -> (Option<u64>, bool) {
    let page_min_seq = records.iter().filter_map(rebuild_record_seq).min();
    if !has_more || records.is_empty() {
        return (None, true);
    }
    if pages_seen >= max_pages {
        return (None, true);
    }
    match page_min_seq {
        // 页内无带 seq 的记录（纯 chunks）→ 无法推进，停（防死循环）。
        None => (None, true),
        Some(min_seq) => {
            // 推进到「严格早于 min_seq」；若游标未前进（重复页）也停。
            if before_seq.is_some_and(|b| min_seq >= b) {
                (None, true)
            } else {
                (Some(min_seq), false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_starts_at_path_pick_and_download_is_single_flight() {
        let mut s = ExportState::default();
        s.open("sess-1", "/tmp/out.zip");
        assert_eq!(s.phase, ExportPhase::PickingPath);
        assert!(s.begin_download());
        assert!(!s.begin_download(), "在途拒绝重复");
        s.mark_progress(100);
        assert_eq!(s.bytes_streamed, 100);
        s.finish();
        assert_eq!(s.phase, ExportPhase::Done);
        assert!(s.begin_download(), "完成后可再次导出");
    }

    #[test]
    fn missing_session_or_path_refuses() {
        let mut s = ExportState::default();
        s.open("sess-1", "");
        assert!(!s.begin_download(), "空路径拒绝");
        let mut s = ExportState::default();
        s.open("", "/tmp/x");
        assert!(!s.begin_download());
        assert!(!s.begin_rebuild(), "重建同样需要 session+path");
    }

    #[test]
    fn cancel_and_fail_leave_recoverable_state() {
        let mut s = ExportState::default();
        s.open("sess-1", "/tmp/out.zip");
        s.begin_download();
        s.cancelled = true;
        s.fail("transport".into());
        assert_eq!(s.phase, ExportPhase::Failed);
        assert_eq!(s.last_error_code.as_deref(), Some("transport"));
        assert!(s.begin_download(), "失败后重试可恢复（幂等）");
    }

    #[test]
    fn rebuild_transitions_and_single_flight_ac007_17() {
        let mut s = ExportState::default();
        s.open("sess-1", "/tmp/out.jsonl");
        // 官方路由失败（Failed）→ 允许转 Rebuilding（D-46 兜底）。
        assert!(s.begin_download());
        s.fail("http-404".into());
        assert_eq!(s.phase, ExportPhase::Failed);
        assert!(s.begin_rebuild(), "404 后进入重建兜底");
        assert_eq!(s.phase, ExportPhase::Rebuilding);
        assert!(!s.begin_rebuild(), "Rebuilding 在途拒绝重复（单飞）");
        s.mark_rebuild_progress(120);
        assert_eq!(s.records_collected, 120);
        s.finish();
        assert_eq!(s.phase, ExportPhase::Done);
        // 重建失败后可重试（幂等恢复路径）。
        let mut s2 = ExportState::default();
        s2.open("sess-1", "/tmp/out.jsonl");
        s2.begin_rebuild();
        s2.fail("transport".into());
        assert!(s2.begin_rebuild(), "重建失败后修正可重跑");
    }

    #[test]
    fn rebuild_jsonl_header_plus_one_line_per_record_ac007_17() {
        use serde_json::json;
        // 输入乱序（含 event 与 chunks 两类 record）→ 输出 header + N 行，
        // 按 seq 顺时，逐行 = 解包/映射后的平铺形态。
        let records = vec![
            json!({"type":"event","event":{"seq":2,"type":"assistant/message"}}),
            json!({"type":"chunks","event":{
                "type":"chunkrow/reasoning-chunks","seq":3,"time":123,
                "data":{"turn":1,"step":1,"index":0,"dt":[1.0],"texts":["x"]}}}),
            json!({"type":"event","event":{"seq":1,"type":"user/message"}}),
        ];
        let out = rebuild_jsonl("sess-1", &records);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), records.len() + 1, "header + N records");
        // header = 官方 session 形态（无 sessionId/source 自造标记）。
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            header,
            json!({"type":"session","version":0,"id":"sess-1"}),
            "header 对齐官方 session 行"
        );
        // 每行可回读、平铺、按 seq 顺时、与输入经解包/映射后一致。
        let expected = [
            json!({"type":"user/message","seq":1}),
            json!({"type":"assistant/message","seq":2}),
            json!({
                "type":"reasoning-chunks","seq0":3,"time0":123,
                "data":{"turn":1,"step":1,"index":0,"dt":[1.0],"texts":["x"]}
            }),
        ];
        for (i, exp) in expected.iter().enumerate() {
            let parsed: serde_json::Value = serde_json::from_str(lines[i + 1]).unwrap();
            assert_eq!(&parsed, exp, "第 {} 行解包/映射对账", i + 1);
        }
    }

    #[test]
    fn rebuild_record_line_unwraps_and_maps_chunkrow_ac008_15() {
        use serde_json::json;
        // event wrapper 解包 → 内层原样（type/seq/time/data 平铺）。
        let event = json!({"type":"event","event":{
            "type":"assistant/chunk","seq":7,"time":99,
            "data":{"turn":1,"step":2,"chunk":{"type":"block-start","index":0}}}});
        let parsed: serde_json::Value =
            serde_json::from_str(rebuild_record_line(&event).trim()).unwrap();
        assert_eq!(
            parsed,
            json!({"type":"assistant/chunk","seq":7,"time":99,
                   "data":{"turn":1,"step":2,"chunk":{"type":"block-start","index":0}}})
        );
        // chunks wrapper → 去 chunkrow/ 前缀 + seq/time → seq0/time0。
        for (chunkrow, official) in [
            ("chunkrow/reasoning-chunks", "reasoning-chunks"),
            ("chunkrow/text-chunks", "text-chunks"),
            ("chunkrow/tool-call-chunks", "tool-call-chunks"),
        ] {
            let chunks = json!({"type":"chunks","event":{
                "type": chunkrow, "seq":14, "time":1788859029701_i64,
                "data":{"turn":1,"step":1,"index":0,"dt":[1.0,2.0],"texts":["a","b"]}}});
            let parsed: serde_json::Value =
                serde_json::from_str(rebuild_record_line(&chunks).trim()).unwrap();
            assert_eq!(
                parsed,
                json!({"type": official, "seq0":14, "time0":1788859029701_i64,
                       "data":{"turn":1,"step":1,"index":0,"dt":[1.0,2.0],"texts":["a","b"]}}),
                "chunkrow {chunkrow} → 官方 {official}"
            );
        }
        // 平铺形态原样（含官方 *-chunks 形态，无 chunkrow/ 前缀 → 不再二次改名）。
        let flat = json!({"type":"reasoning-chunks","seq0":5,"time0":1,"data":{"texts":[]}});
        let parsed: serde_json::Value =
            serde_json::from_str(rebuild_record_line(&flat).trim()).unwrap();
        assert_eq!(parsed, flat);
    }

    #[test]
    fn rebuild_sort_chronological_orders_by_seq_ac008_15() {
        use serde_json::json;
        let mut recs = vec![
            json!({"type":"event","event":{"seq":50,"type":"step/end"}}),
            json!({"type":"event","event":{"seq":1,"type":"permission/preset"}}),
            json!({"type":"chunks","event":{
                "type":"chunkrow/text-chunks","seq":25,"time":1,"data":{"texts":[]}}}),
            json!({"type":"event","event":{"seq":10,"type":"step/start"}}),
        ];
        rebuild_sort_chronological(&mut recs);
        let seqs: Vec<u64> = recs
            .iter()
            .map(|r| r["event"]["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![1, 10, 25, 50], "按 seq 升序（顺时）");
    }

    #[test]
    fn rebuild_cursor_walks_oldest_first_and_stops_ac007_17() {
        use serde_json::json;
        let p1 = vec![
            json!({"type":"event","event":{"seq":9}}),
            json!({"type":"event","event":{"seq":8}}),
            json!({"type":"event","event":{"seq":7}}),
        ];
        // 首页 has_more → 以最旧 7 为下一 beforeSeq（独占上界：下次返回 seq<7）。
        let (next, stop) = rebuild_next_cursor(None, &p1, true, 0, 100);
        assert_eq!(next, Some(7));
        assert!(!stop);
        // 第二页 seq 6..4 has_more=false → 停。
        let p2 = vec![
            json!({"type":"event","event":{"seq":6}}),
            json!({"type":"event","event":{"seq":4}}),
        ];
        let (_, stop2) = rebuild_next_cursor(next, &p2, false, 1, 100);
        assert!(stop2, "has_more=false 停止");
        // 空页 / 无 seq（纯 chunks）/ 达页数上界 → 停（防死循环）。
        assert!(rebuild_next_cursor(Some(6), &[], true, 2, 100).1);
        assert!(
            rebuild_next_cursor(
                Some(6),
                &[json!({"type":"chunks","event":{}})],
                true,
                3,
                100
            )
            .1
        );
        let p3 = vec![json!({"type":"event","event":{"seq":3}})];
        assert!(
            rebuild_next_cursor(None, &p3, true, 100, 100).1,
            "页数硬上界"
        );
        // 游标未前进（重复页/无更新）→ 停。
        let dup = vec![json!({"type":"event","event":{"seq":5}})];
        assert!(
            rebuild_next_cursor(Some(5), &dup, true, 0, 100).1,
            "min_seq>=before 停"
        );
    }
}
