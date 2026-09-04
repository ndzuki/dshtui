//! V0.1 skeleton for the session search index (REQ-001 §5: `SearchIndex` is a
//! V0.1 placeholder; the full content index belongs to REQ-003).
//!
//! V0.1 only maintains session membership so REQ-003 can attach the real
//! inverted index on top without changing WorkspaceStore consumers.

use crate::api::types::SessionId;

#[derive(Debug, Default)]
pub struct SearchIndex {
    sessions: Vec<SessionId>,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a session in the index (idempotent).
    pub fn upsert(&mut self, id: SessionId) {
        if !self.sessions.contains(&id) {
            self.sessions.push(id);
        }
    }

    /// Indexed session ids (stable insertion order).
    pub fn session_ids(&self) -> &[SessionId] {
        &self.sessions
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_index_is_idempotent_on_upsert() {
        let mut index = SearchIndex::new();
        index.upsert(SessionId("s1".into()));
        index.upsert(SessionId("s2".into()));
        index.upsert(SessionId("s1".into()));
        assert_eq!(index.len(), 2);
        assert_eq!(index.session_ids()[0], SessionId("s1".into()));
        assert_eq!(index.session_ids()[1], SessionId("s2".into()));
    }
}
