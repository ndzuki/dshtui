//! Plan-mode indicator state (REQ-007 AC-007-26; pure model).
//!
//! Wire correction: the `plan` projection is ONLY `{active:boolean,
//! pending:boolean}` — there is no deliverables/plan-content projection
//! (web deliverables are derived client-side from turn events). The TUI shows
//! the plan-mode state read-only; no fabricated plan content (ADR-008).

/// Plan panel state (read-only projection mirror).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanViewState {
    pub visible: bool,
    /// Official projection values (None = projection absent → empty + guide).
    pub active: Option<bool>,
    pub pending: Option<bool>,
}

impl PlanViewState {
    pub fn open(&mut self) {
        self.visible = true;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    pub fn set_projection(&mut self, active: Option<bool>, pending: Option<bool>) {
        self.active = active;
        self.pending = pending;
    }

    /// Display status: "plan" | "switching" | "off" | "unsupported".
    pub fn status(&self) -> &'static str {
        match (self.active, self.pending) {
            (Some(true), _) => "plan",
            (Some(false), Some(true)) => "switching",
            (Some(false), _) => "off",
            (None, _) => "unsupported",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_status_matrix() {
        let mut s = PlanViewState::default();
        s.set_projection(Some(true), Some(false));
        assert_eq!(s.status(), "plan");
        s.set_projection(Some(false), Some(true));
        assert_eq!(s.status(), "switching");
        s.set_projection(Some(false), Some(false));
        assert_eq!(s.status(), "off");
        // 无 plan 投影（未合成）→ 空态 + 指引，不伪造内容。
        let s = PlanViewState::default();
        assert_eq!(s.status(), "unsupported");
    }
}
