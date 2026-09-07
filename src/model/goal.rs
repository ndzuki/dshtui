//! Goal panel state (REQ-007 FR-007-02 half; wire correction: a goal is a
//! per-session SINGLETON, not a list — there is no list/query endpoint).
//!
//! Pure model (no IO). All mutations carry the current `revision` as a CAS;
//! a `GOAL_STALE_REVISION` failure invalidates the cached revision so the UI
//! re-reads the `goal` projection before retrying.

use crate::api::types::GoalPhase;

/// One goal as displayed in the panel (mirror of the official snapshot).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GoalView {
    pub id: String,
    pub revision: u64,
    pub objective: String,
    pub phase: Option<GoalPhase>,
    pub blocked_reason: Option<String>,
    pub max_goal_rounds: Option<u64>,
    /// roundsStarted / createdAt / updatedAt from the projection (optional).
    pub rounds_started: Option<u64>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

/// In-flight goal mutation kinds (single-flight; CAS ref captured at send).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalOpKind {
    Create,
    Edit,
    Pause,
    Resume,
    Complete,
    Clear,
}

impl GoalOpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalOpKind::Create => "create",
            GoalOpKind::Edit => "edit",
            GoalOpKind::Pause => "pause",
            GoalOpKind::Resume => "resume",
            GoalOpKind::Complete => "complete",
            GoalOpKind::Clear => "clear",
        }
    }
}

/// Goal panel state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GoalPanelState {
    pub visible: bool,
    /// Current singleton goal (None = empty state).
    pub goal: Option<GoalView>,
    /// CAS revision snapshot captured when the last mutation was sent.
    pub sent_revision: Option<u64>,
    /// In-flight op (single-flight; a second op is refused while Some).
    pub inflight: Option<GoalOpKind>,
    /// Stale-CAS flag: after GOAL_STALE_REVISION the panel must re-read the
    /// projection before sending again.
    pub stale_revision: bool,
    /// Create flow: objective input buffer (sub-stage).
    pub create_objective: String,
    pub create_max_rounds: Option<u64>,
    /// Clear confirm sub-stage (ConfirmDanger precedent).
    pub confirm_pending: Option<GoalOpKind>,
    pub last_error_code: Option<String>,
}

impl GoalPanelState {
    pub fn open(&mut self) {
        self.visible = true;
        self.last_error_code = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// Merge a projection `goal` snapshot (or clear on null).
    pub fn set_goal(&mut self, goal: Option<GoalView>, stale: bool) {
        self.goal = goal;
        self.stale_revision = stale;
    }

    /// Begin a mutation (returns false when one is already in-flight or the
    /// revision is stale — the UI must re-read the projection first).
    pub fn begin_op(&mut self, kind: GoalOpKind) -> bool {
        if self.inflight.is_some() {
            return false;
        }
        match kind {
            GoalOpKind::Create => {
                if self.goal.is_some() && !self.stale_revision {
                    return false; // 单例：已有 goal 不能 create
                }
            }
            _ => {
                if self.goal.is_none() || self.stale_revision {
                    return false;
                }
                self.sent_revision = self.goal.as_ref().map(|g| g.revision);
            }
        }
        self.inflight = Some(kind);
        self.last_error_code = None;
        true
    }

    pub fn settle_op(&mut self, updated: Option<GoalView>) {
        let was_clear = self.inflight == Some(GoalOpKind::Clear);
        self.inflight = None;
        self.sent_revision = None;
        self.confirm_pending = None;
        if let Some(g) = updated {
            self.set_goal(Some(g), false);
        } else if was_clear {
            self.set_goal(None, false);
        }
    }

    pub fn fail_op(&mut self, code: String, stale: bool) {
        self.inflight = None;
        if stale {
            self.stale_revision = true;
        }
        self.last_error_code = Some(code);
    }

    /// Clear has a ConfirmDanger sub-stage: request confirm, only the confirm
    /// action issues the op.
    pub fn request_confirm(&mut self, kind: GoalOpKind) {
        self.confirm_pending = Some(kind);
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm_pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(revision: u64, phase: GoalPhase) -> GoalView {
        GoalView {
            id: "g1".into(),
            revision,
            objective: "交付 REQ-007".into(),
            phase: Some(phase),
            ..Default::default()
        }
    }

    #[test]
    fn singleton_semantics_create_blocked_when_goal_exists() {
        let mut s = GoalPanelState::default();
        s.set_goal(Some(goal(1, GoalPhase::Active)), false);
        assert!(!s.begin_op(GoalOpKind::Create), "单例已有 goal 拒绝 create");
        assert!(s.begin_op(GoalOpKind::Pause));
        assert!(!s.begin_op(GoalOpKind::Complete), "在途拒绝第二 op");
        s.settle_op(Some(goal(2, GoalPhase::Paused)));
        assert_eq!(s.inflight, None);
    }

    #[test]
    fn stale_revision_blocks_ops_until_goal_read() {
        let mut s = GoalPanelState::default();
        s.set_goal(Some(goal(1, GoalPhase::Active)), false);
        assert!(s.begin_op(GoalOpKind::Pause));
        s.fail_op("GOAL_STALE_REVISION".into(), true);
        assert!(s.stale_revision);
        assert!(!s.begin_op(GoalOpKind::Resume), "stale 时禁止再发");
        // 重读投影后解除 stale（未 stale 语义）。
        s.set_goal(Some(goal(3, GoalPhase::Paused)), false);
        assert!(!s.stale_revision);
        assert!(s.begin_op(GoalOpKind::Resume));
    }

    #[test]
    fn clear_requires_confirm_stage() {
        let mut s = GoalPanelState::default();
        s.set_goal(Some(goal(1, GoalPhase::Paused)), false);
        s.request_confirm(GoalOpKind::Clear);
        assert_eq!(s.confirm_pending, Some(GoalOpKind::Clear));
        assert_eq!(s.inflight, None, "确认前不发");
        assert!(s.begin_op(GoalOpKind::Clear), "确认后才发");
        s.settle_op(None);
        assert_eq!(s.goal, None, "clear 成功清空单例");
    }
}
