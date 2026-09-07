//! Message action state (REQ-007 AC-007-27/28; pure model).
//!
//! Wire corrections: retry has NO RPC — retry = re-issue `session/prompt`
//! with a NEW requestId and the same user content; branch = `session/fork
//! atSeq` (existing wrapper, only the last message of a still turn); feedback
//! = `messageFeedback/put` (CAS ifVersion; messageId = assistant message id).
//! Actions only target STATIC (non-running) nodes; running nodes prompt for
//! confirmation first (AC-007-28).

/// Message action kinds offered on a static message row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageActionKind {
    Branch,
    Retry,
    FeedbackPositive,
    FeedbackNegative,
}

impl MessageActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageActionKind::Branch => "branch",
            MessageActionKind::Retry => "retry",
            MessageActionKind::FeedbackPositive => "feedback+",
            MessageActionKind::FeedbackNegative => "feedback-",
        }
    }
}

/// Message action state (per-row actions are single-flight + requestId
/// idempotent; a running turn confirms before acting).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageActionState {
    /// Action menu open for this message block seq.
    pub menu_seq: Option<u64>,
    pub menu_cursor: usize,
    /// In-flight action (single-flight).
    pub inflight: Option<MessageActionKind>,
    /// Running-node confirm (AC-007-28).
    pub confirm_running: bool,
    /// Optional feedback note input.
    pub feedback_note: String,
    pub last_error_code: Option<String>,
}

impl MessageActionState {
    pub fn open_menu(&mut self, seq: u64) {
        self.menu_seq = Some(seq);
        self.menu_cursor = 0;
        self.last_error_code = None;
    }

    pub fn close_menu(&mut self) {
        self.menu_seq = None;
        self.confirm_running = false;
        self.feedback_note.clear();
    }

    /// Begin an action. `running` = the target message belongs to a running
    /// turn — must confirm first (returned via confirm_running); otherwise
    /// single-flight begins now.
    pub fn begin(&mut self, kind: MessageActionKind, running: bool) -> bool {
        if self.inflight.is_some() {
            return false;
        }
        if running {
            self.confirm_running = true;
            return false;
        }
        self.inflight = Some(kind);
        self.last_error_code = None;
        true
    }

    /// Confirm running-node action (AC-007-28 second step).
    pub fn confirm(&mut self, kind: MessageActionKind) -> bool {
        if !self.confirm_running || self.inflight.is_some() {
            return false;
        }
        self.confirm_running = false;
        self.inflight = Some(kind);
        true
    }

    pub fn settle(&mut self) {
        self.inflight = None;
        self.close_menu();
    }

    pub fn fail(&mut self, code: String) {
        self.inflight = None;
        self.last_error_code = Some(code);
    }
}

/// Only the LAST user message of a still (non-running) session may branch.
/// `is_last_user` is provided by the caller from the window; this helper keeps
/// the guard explicit.
pub fn can_branch(running: bool, is_last_user: bool) -> bool {
    !running && is_last_user
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_and_close_menu() {
        let mut s = MessageActionState::default();
        s.open_menu(7);
        assert_eq!(s.menu_seq, Some(7));
        s.close_menu();
        assert_eq!(s.menu_seq, None);
    }

    #[test]
    fn static_node_single_flight_begins_immediately() {
        let mut s = MessageActionState::default();
        assert!(s.begin(MessageActionKind::Retry, false));
        assert!(!s.begin(MessageActionKind::Branch, false), "在途拒绝");
        s.fail("transport".into());
        assert!(s.begin(MessageActionKind::Branch, false), "失败后可重试");
    }

    #[test]
    fn running_node_confirms_then_acts() {
        let mut s = MessageActionState::default();
        assert!(!s.begin(MessageActionKind::Retry, true), "运行中不直接发");
        assert!(s.confirm_running);
        assert!(s.confirm(MessageActionKind::Retry), "确认后才发");
        assert_eq!(s.inflight, Some(MessageActionKind::Retry));
        s.settle();
        assert!(!s.confirm_running);
    }

    #[test]
    fn branch_only_on_static_last_user() {
        assert!(can_branch(false, true));
        assert!(!can_branch(true, true), "运行中不可分支");
        assert!(!can_branch(false, false), "非末条 user 不可分支");
    }
}
