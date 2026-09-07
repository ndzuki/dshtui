//! Export state (REQ-007 FR-007-04; pure model).
//!
//! Primary (and only production) path = official same-origin HTTP
//! `/api/session.export` ZIP download (byte-identical). The page-rebuild
//! JSONL fallback is deliberately NOT wired (deferred ~nice-to-have,
//! TASK-007 Step 14 scope decision): the official route is always present on
//! the target backend, and the fallback's exact session-log-export line
//! format needs a live contract smoke that cannot run headless — shipping an
//! unverifiable format would be worse than none. One export per session is
//! single-flight (requestId idempotent).

/// Export phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportPhase {
    #[default]
    Idle,
    /// Confirm-path sub-stage (write to a user-chosen path).
    PickingPath,
    Downloading,
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

    pub fn mark_progress(&mut self, bytes: u64) {
        self.bytes_streamed = bytes;
    }

    pub fn finish(&mut self) {
        self.phase = ExportPhase::Done;
    }

    pub fn fail(&mut self, code: String) {
        self.phase = ExportPhase::Failed;
        self.last_error_code = Some(code);
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
}
