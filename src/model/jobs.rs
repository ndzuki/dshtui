//! Jobs panel state (REQ-007 FR-007-02 half; wire correction: read-only —
//! 0.1.2-rc.1 has NO job stop endpoint and the official web jobs panel has no
//! stop action).
//!
//! The mirror is maintained from `session/control` frames: baseline
//! `jobs` per-session + `jobs` replacement frames REPLACE the whole mirror
//! (never incremental). Empty array clears the mirror — no fabricated
//! numbers (ADR-008).

use crate::api::types::{SessionJob, SessionJobStatus};

/// Jobs panel state (mirror + view selection).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobsPanelState {
    pub visible: bool,
    /// Full replacement mirror (latest control frame wins).
    pub jobs: Vec<SessionJob>,
    pub selected: usize,
    pub last_error_code: Option<String>,
}

impl JobsPanelState {
    pub fn open(&mut self) {
        self.visible = true;
        self.last_error_code = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// Full replacement (baseline.jobs / `jobs` frame). Unknown rows are kept
    /// raw-tolerant by the api parse; this fold is a straight swap.
    pub fn replace(&mut self, jobs: Vec<SessionJob>) {
        self.jobs = jobs;
        if !self.jobs.is_empty() {
            self.selected = self.selected.min(self.jobs.len() - 1);
        } else {
            self.selected = 0;
        }
    }

    /// Number of running/stopping jobs (status-bar badge; read-only mirror —
    /// ADR-008 never self-computes totals beyond this mirror count).
    pub fn active_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|j| {
                matches!(
                    j.status,
                    Some(SessionJobStatus::Running) | Some(SessionJobStatus::Stopping)
                )
            })
            .count()
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.jobs.is_empty() {
            return;
        }
        let len = self.jobs.len() as isize;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len - 1) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SessionJob;

    fn job(id: &str, status: SessionJobStatus) -> SessionJob {
        SessionJob {
            id: id.into(),
            kind: "k".into(),
            label: format!("job {id}"),
            status: Some(status),
            ..Default::default()
        }
    }

    #[test]
    fn replacement_semantics_full_swap_and_empty_clears() {
        let mut s = JobsPanelState::default();
        s.replace(vec![
            job("j1", SessionJobStatus::Running),
            job("j2", SessionJobStatus::Completed),
        ]);
        assert_eq!(s.jobs.len(), 2);
        assert_eq!(s.active_count(), 1, "running 计入 active");
        // 替换帧全量替换（旧 j1 消失）。
        s.replace(vec![job("j3", SessionJobStatus::Running)]);
        assert_eq!(s.jobs.len(), 1);
        assert_eq!(s.jobs[0].id, "j3");
        // 空数组清镜像（不伪造数字）。
        s.replace(vec![]);
        assert!(s.jobs.is_empty());
        assert_eq!(s.active_count(), 0);
    }

    #[test]
    fn selection_clamped_and_movable() {
        let mut s = JobsPanelState::default();
        s.replace(vec![
            job("j1", SessionJobStatus::Running),
            job("j2", SessionJobStatus::Failed),
        ]);
        s.move_selection(1);
        assert_eq!(s.selected, 1);
        s.move_selection(5);
        assert_eq!(s.selected, 1, "越界 clamp");
        s.move_selection(-10);
        assert_eq!(s.selected, 0);
        s.replace(vec![]);
        s.move_selection(1); // no panic on empty
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn stopping_is_active_like_running() {
        let mut s = JobsPanelState::default();
        s.replace(vec![
            job("j1", SessionJobStatus::Stopping),
            job("j2", SessionJobStatus::Killed),
            job("j3", SessionJobStatus::Failed),
        ]);
        assert_eq!(
            s.active_count(),
            1,
            "stopping 计入 active；killed/failed 不计"
        );
    }
}
