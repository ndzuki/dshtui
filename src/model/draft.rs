//! Cross-session draft registry + global input history (REQ-003 FR-003-04,
//! D-20; extends REQ-002 §5 `DraftState`). Memory only — nothing is persisted
//! (Notes/06 §9), process exit loses drafts by design.

use std::collections::{HashMap, VecDeque};

use crate::api::types::SessionId;

/// Session-bound draft (REQ-002 §5 fields; REQ-003 D-20 makes the registry
/// the owner of cross-session retention).
#[derive(Debug, Clone, PartialEq)]
pub struct DraftState {
    pub text: String,
    /// Cursor position (char offset).
    pub cursor: usize,
    pub bound_session: SessionId,
}

/// Per-session drafts: at most `cap` sessions, FIFO eviction on overflow
/// (AC-003-11; Notes/06 §7 memory budget).
#[derive(Debug, Clone)]
pub struct DraftRegistry {
    map: HashMap<SessionId, DraftState>,
    order: VecDeque<SessionId>,
    cap: usize,
}

impl DraftRegistry {
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    pub fn get(&self, sid: &SessionId) -> Option<&DraftState> {
        self.map.get(sid)
    }

    /// Store (or replace) a draft; moves the session to the MRU end and
    /// evicts the least-recently-used session when over capacity.
    pub fn set(&mut self, draft: DraftState) {
        let sid = draft.bound_session.clone();
        let inserted = self.map.insert(sid.clone(), draft).is_none();
        if inserted {
            self.order.push_back(sid);
            if self.order.len() > self.cap {
                if let Some(evicted) = self.order.pop_front() {
                    self.map.remove(&evicted);
                }
            }
        }
    }

    /// Remove and return a draft (send path clears it; AC-003-11 keeps it on
    /// Esc).
    pub fn take(&mut self, sid: &SessionId) -> Option<DraftState> {
        let out = self.map.remove(sid)?;
        self.order.retain(|s| s != sid);
        Some(out)
    }

    pub fn clear(&mut self, sid: &SessionId) {
        if self.map.remove(sid).is_some() {
            self.order.retain(|s| s != sid);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// All session ids currently holding a draft (for persistence snapshot /
    /// startup clear, AC-007-22).
    pub fn session_ids(&self) -> Vec<SessionId> {
        self.order.iter().cloned().collect()
    }

    /// Clear every session (startup `[drafts].clear` / disable path).
    pub fn clear_all(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

impl Default for DraftRegistry {
    fn default() -> Self {
        Self::new(20)
    }
}

/// Global input history (REQ-F06: most recent 50 entries, FIFO, memory only).
#[derive(Debug, Clone, Default)]
pub struct InputHistory {
    entries: VecDeque<String>,
    cap: usize,
    /// Navigation cursor: `None` = not navigating (live draft shown).
    cursor: Option<usize>,
    /// Draft text saved when navigation started (↓ restores it).
    original: Option<String>,
}

impl InputHistory {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cap: cap.max(1),
            cursor: None,
            original: None,
        }
    }

    /// Record a sent prompt (dedupe consecutive identical entries, FIFO cap).
    pub fn push(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if self.entries.back().is_some_and(|last| last == text) {
            return;
        }
        self.entries.push_back(text.to_string());
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    /// `↑`: move one entry back; starting a fresh navigation snapshots the
    /// live draft first.
    pub fn prev(&mut self, current: &str) -> Option<&str> {
        if self.cursor.is_none() {
            self.original = Some(current.to_string());
            self.cursor = Some(self.entries.len());
        }
        let cur = self.cursor.unwrap_or(self.entries.len());
        if cur > 0 {
            self.cursor = Some(cur - 1);
            return self.entries.get(cur - 1).map(String::as_str);
        }
        self.entries.front().map(String::as_str)
    }

    /// `↓`: move one entry forward; past the newest restores the live draft.
    pub fn next_entry(&mut self) -> Option<&str> {
        let cur = self.cursor?;
        if cur + 1 >= self.entries.len() {
            self.cursor = None;
            return self.original.as_deref();
        }
        self.cursor = Some(cur + 1);
        self.entries.get(cur + 1).map(String::as_str)
    }

    /// Reset navigation (e.g. after send) so the next ↑ snapshots fresh.
    pub fn reset_nav(&mut self) {
        self.cursor = None;
        self.original = None;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: &str) -> SessionId {
        SessionId(n.into())
    }

    #[test]
    fn registry_keeps_draft_across_session_switches_ac003_11() {
        let mut reg = DraftRegistry::new(20);
        reg.set(DraftState {
            text: "草稿A".into(),
            cursor: 3,
            bound_session: sid("s1"),
        });
        // Switch away and back: the draft is still there.
        assert_eq!(reg.get(&sid("s1")).map(|d| d.text.as_str()), Some("草稿A"));
        // Send path clears it.
        assert!(reg.take(&sid("s1")).is_some());
        assert!(reg.get(&sid("s1")).is_none());
    }

    #[test]
    fn registry_evicts_lru_over_capacity() {
        let mut reg = DraftRegistry::new(2);
        for n in ["s1", "s2", "s3"] {
            reg.set(DraftState {
                text: n.into(),
                cursor: 0,
                bound_session: sid(n),
            });
        }
        assert_eq!(reg.len(), 2);
        assert!(reg.get(&sid("s1")).is_none(), "最旧会话被逐出");
        assert!(reg.get(&sid("s3")).is_some());
    }

    #[test]
    fn input_history_navigates_and_restores_ac003_10() {
        let mut h = InputHistory::new(50);
        h.push("第一条");
        h.push("第二条");
        assert_eq!(h.prev("正在编辑"), Some("第二条"));
        assert_eq!(h.prev("正在编辑"), Some("第一条"));
        assert_eq!(h.prev("正在编辑"), Some("第一条"), "顶部停留");
        assert_eq!(h.next_entry(), Some("第二条"));
        assert_eq!(h.next_entry(), Some("正在编辑"), "越新即恢复原草稿");
    }

    #[test]
    fn input_history_dedupes_consecutive_and_caps() {
        let mut h = InputHistory::new(2);
        h.push("a");
        h.push("a");
        assert_eq!(h.len(), 1, "连续重复去重");
        h.push("b");
        h.push("c");
        assert_eq!(h.len(), 2, "FIFO 上限");
        h.reset_nav();
        assert_eq!(h.prev("x"), Some("c"));
    }
}
