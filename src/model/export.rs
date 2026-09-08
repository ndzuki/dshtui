//! Export state (REQ-007 FR-007-04; pure model).
//!
//! Primary path = official same-origin HTTP `/api/session.export` ZIP download
//! (byte-identical, D-41). Fallback (D-46 HARD contract): when the official
//! route is unavailable/fails (404/5xx/transport), rebuild a JSONL export from
//! `session/page` full-history records. 401/403 (permission) NEVER falls back
//! and is never auto-retried. The fallback's byte-level line format is
//! `[验证]` (no local contract evidence; REQ-008 live smoke locks it); the
//! measurable acceptance is records/key-event count vs the official export.
//! One export per session is single-flight (requestId idempotent).

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

/// D-46 page 重建 JSONL header 行（会话 meta；`[验证]` 格式待 REQ-008）。
pub fn rebuild_header_line(session_id: &str) -> String {
    let header = serde_json::json!({
        "type": "session-meta",
        "sessionId": session_id,
        "source": "page-rebuild",
    });
    let mut out = serde_json::to_string(&header).unwrap_or_else(|_| "{}".into());
    out.push('\n');
    out
}

/// D-46 page 重建 JSONL 单条 record 行（compact；损坏值以 `{}` 行兜底）。
pub fn rebuild_record_line(rec: &serde_json::Value) -> String {
    let mut out = serde_json::to_string(rec).unwrap_or_else(|_| "{}".into());
    out.push('\n');
    out
}

/// D-46 page 重建 JSONL 纯函数（headless seam）：`records`（fixture/收集的
/// 原始 record JSON）→ 输出 JSONL 文本——header 行（会话 meta）+ 每行一条
/// record。行格式 `[验证]`（无 live 契约证据，REQ-008 冒烟锁定）；可测口径 =
/// 行数 = records 数 + header，内容逐条可对账。
pub fn rebuild_jsonl(session_id: &str, records: &[serde_json::Value]) -> String {
    let mut out = rebuild_header_line(session_id);
    for rec in records {
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
/// 返回 (next_before_seq, should_stop)。`[验证]`：live 冒烟在 REQ-008 锁定，
/// mock 语义与官方 client 用法一致。
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

/// 从一条 record 的原始 JSON 提取 seq：Event 形态 `{type:"event",event:{seq}}`
/// 与宽容 `{seq}` 兜底；Chunks 无 seq。
fn rebuild_record_seq(rec: &serde_json::Value) -> Option<u64> {
    let seq = rec
        .get("event")
        .and_then(|e| e.get("seq"))
        .or_else(|| rec.get("seq"))
        .and_then(|v| v.as_u64());
    seq
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
        let records = vec![
            serde_json::json!({"type":"event","event":{"seq":1,"type":"user/message"}}),
            serde_json::json!({"type":"event","event":{"seq":2,"type":"assistant/message"}}),
        ];
        let out = rebuild_jsonl("sess-1", &records);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), records.len() + 1, "header + N records");
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["type"], "session-meta");
        assert_eq!(header["sessionId"], "sess-1");
        // 每行可回读且与输入一致。
        for (i, rec) in records.iter().enumerate() {
            let parsed: serde_json::Value = serde_json::from_str(lines[i + 1]).unwrap();
            assert_eq!(parsed, *rec, "第 {i} 行内容对账");
        }
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
