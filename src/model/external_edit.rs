//! External editor suspend state (REQ-007 AC-007-25; pure model).
//!
//! `:edit` suspends the main loop (TerminalSession leaves raw mode), runs
//! `$EDITOR` on a temp file, then refills the composer. The temp file is
//! created inside the SAME filesystem as the target (never /tmp across
//! commands — `uncategorized/TASK-002-pitfall`).

/// `:edit` flow state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExternalEditPhase {
    #[default]
    Inactive,
    /// Editor subprocess running (raw mode released).
    Editing,
    /// Editor exited ok; waiting for main loop to refill composer.
    RefillPending,
}

/// External edit state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExternalEditState {
    pub phase: ExternalEditPhase,
    /// Suspended composer draft text (restored if the user cancels).
    pub suspended_text: String,
    /// Temp file path for the current edit (same-filesystem, cleaned after).
    pub tmp_path: Option<String>,
    /// Resolved editor command (config `$EDITOR` fallback).
    pub editor: Option<String>,
    /// Last error (editor missing / abnormal exit) → readable message, safe
    /// return to composer.
    pub last_error: Option<String>,
}

impl ExternalEditState {
    /// Suspend: capture the current draft and the temp path. Returns false if
    /// an edit is already running (single-flight).
    pub fn suspend(&mut self, draft_text: &str, tmp_path: String, editor: Option<String>) -> bool {
        if self.phase != ExternalEditPhase::Inactive {
            return false;
        }
        self.suspended_text = draft_text.to_string();
        self.tmp_path = Some(tmp_path);
        self.editor = editor;
        self.phase = ExternalEditPhase::Editing;
        self.last_error = None;
        true
    }

    /// Editor exited successfully → refill pending.
    pub fn mark_exited_ok(&mut self) {
        self.phase = ExternalEditPhase::RefillPending;
    }

    /// Refill done → back to inactive (temp cleaned by the caller).
    pub fn settle(&mut self) {
        *self = Self::default();
    }

    /// Cancel path: keep the original draft, no refill.
    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    pub fn fail(&mut self, message: String) {
        self.phase = ExternalEditPhase::Inactive;
        self.tmp_path = None;
        self.last_error = Some(message);
    }

    pub fn is_active(&self) -> bool {
        self.phase != ExternalEditPhase::Inactive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspend_captures_and_refill_settles() {
        let mut s = ExternalEditState::default();
        assert!(s.suspend("原草稿", "/tmp/draft.md".into(), Some("vim".into())));
        assert_eq!(s.phase, ExternalEditPhase::Editing);
        assert_eq!(s.suspended_text, "原草稿");
        assert!(!s.suspend("x", "/y".into(), None), "在途拒绝第二个 edit");
        s.mark_exited_ok();
        assert_eq!(s.phase, ExternalEditPhase::RefillPending);
        s.settle();
        assert_eq!(s.phase, ExternalEditPhase::Inactive);
    }

    #[test]
    fn cancel_preserves_original_and_clears_temp() {
        let mut s = ExternalEditState::default();
        s.suspend("原草稿", "/tmp/draft.md".into(), None);
        s.cancel();
        assert_eq!(s.phase, ExternalEditPhase::Inactive);
        assert_eq!(s.suspended_text, "");
        assert_eq!(s.tmp_path, None);
    }

    #[test]
    fn failure_sets_readable_error_and_safe_state() {
        let mut s = ExternalEditState::default();
        s.suspend(
            "原草稿",
            "/tmp/draft.md".into(),
            Some("nonexistent-editor".into()),
        );
        s.fail("编辑器不存在或异常退出".into());
        assert_eq!(s.phase, ExternalEditPhase::Inactive);
        assert_eq!(s.last_error.as_deref(), Some("编辑器不存在或异常退出"));
    }
}
