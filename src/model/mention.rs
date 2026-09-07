//! @-mention state (REQ-007 AC-007-23; pure model).
//!
//! Two candidate sources (wire fact): `fileReferences/list` (files/dirs) +
//! `sessionReferenceResolver/candidates` (sessions). Local nucleo filtering
//! + throttle; fetch failure degrades to manual typing (no crash).

use crate::api::types::{FileReferenceCandidate, SessionReferenceMentionCandidate};

/// Mention candidate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    File,
    Session,
}

/// One normalized mention candidate row.
#[derive(Debug, Clone, PartialEq)]
pub struct MentionCandidate {
    pub kind: MentionKind,
    /// File path or session label (display).
    pub display: String,
    /// Insert text: file path or official mention text.
    pub insert: String,
}

impl From<FileReferenceCandidate> for MentionCandidate {
    fn from(c: FileReferenceCandidate) -> Self {
        Self {
            kind: MentionKind::File,
            display: c.path.clone(),
            insert: c.path,
        }
    }
}

impl From<SessionReferenceMentionCandidate> for MentionCandidate {
    fn from(c: SessionReferenceMentionCandidate) -> Self {
        Self {
            kind: MentionKind::Session,
            display: if c.label.is_empty() {
                c.session_id.clone()
            } else {
                format!("{} ({})", c.label, c.session_id)
            },
            insert: if c.mention.is_empty() {
                format!("@{}", c.session_id)
            } else {
                c.mention
            },
        }
    }
}

/// Mention panel state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MentionState {
    pub active: bool,
    pub query: String,
    pub candidates: Vec<MentionCandidate>,
    pub selected: usize,
    /// Whether a fetch is in flight (throttle + generation guard).
    pub loading: bool,
    pub generation: u64,
    pub last_error_code: Option<String>,
}

impl MentionState {
    pub fn activate(&mut self) {
        self.active = true;
        self.query = String::new();
        self.candidates = Vec::new();
        self.selected = 0;
        self.last_error_code = None;
    }

    pub fn deactivate(&mut self) {
        *self = Self::default();
    }

    /// Set/update the query, keep local filter results.
    pub fn set_query(&mut self, q: String) {
        self.query = q;
        self.selected = 0;
    }

    pub fn set_candidates(
        &mut self,
        generation: u64,
        files: Vec<FileReferenceCandidate>,
        sessions: Vec<SessionReferenceMentionCandidate>,
    ) {
        if generation != self.generation {
            return; // stale drop（generation 守卫）
        }
        self.loading = false;
        let mut out = Vec::new();
        out.extend(files.into_iter().map(MentionCandidate::from));
        out.extend(sessions.into_iter().map(MentionCandidate::from));
        self.candidates = out;
        self.selected = 0;
    }

    pub fn mark_loading(&mut self) {
        self.loading = true;
        self.generation += 1;
    }

    pub fn fail(&mut self, code: String) {
        self.loading = false;
        // 失败降级：保留已缓存的候选（若有），仅记录错误可手动输入不崩。
        self.last_error_code = Some(code);
    }

    /// Local filter (already fetched candidates narrowed by query).
    pub fn filtered(&self) -> Vec<&MentionCandidate> {
        let q = self.query.to_lowercase();
        self.candidates
            .iter()
            .filter(|c| q.is_empty() || c.display.to_lowercase().contains(&q))
            .collect()
    }

    pub fn move_selection(&mut self, delta: isize) {
        let rows = self.filtered();
        if rows.is_empty() {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_two_sources_and_filters_locally() {
        let mut m = MentionState::default();
        m.activate();
        m.mark_loading();
        m.set_candidates(
            m.generation,
            vec![FileReferenceCandidate {
                path: "src/api/mod.rs".into(),
                kind: "file".into(),
            }],
            vec![SessionReferenceMentionCandidate {
                session_id: "s1".into(),
                label: "部署排查".into(),
                cwd: None,
                same_workspace: true,
                created_at: None,
                mention: "@[部署排查](dsh-session:s1)".into(),
            }],
        );
        assert_eq!(m.candidates.len(), 2);
        assert_eq!(m.filtered().len(), 2);
        m.set_query("mod.rs".into());
        assert_eq!(m.filtered().len(), 1);
        assert_eq!(m.filtered()[0].kind, MentionKind::File);
        m.set_query("部署".into());
        assert_eq!(m.filtered()[0].kind, MentionKind::Session);
        assert_eq!(m.filtered()[0].insert, "@[部署排查](dsh-session:s1)");
    }

    #[test]
    fn stale_generation_dropped() {
        let mut m = MentionState::default();
        m.activate();
        m.mark_loading(); // generation 1
        m.set_candidates(
            0, // 旧 generation 迟到
            vec![FileReferenceCandidate {
                path: "old.rs".into(),
                kind: "file".into(),
            }],
            vec![],
        );
        assert!(m.candidates.is_empty(), "stale 响应丢弃");
        m.set_candidates(
            m.generation,
            vec![FileReferenceCandidate {
                path: "new.rs".into(),
                kind: "file".into(),
            }],
            vec![],
        );
        assert_eq!(m.candidates.len(), 1);
        assert!(!m.loading);
    }

    #[test]
    fn fetch_failure_degrades_to_empty_with_error() {
        let mut m = MentionState::default();
        m.activate();
        m.mark_loading();
        m.fail("transport".into());
        assert!(!m.loading);
        assert_eq!(m.last_error_code.as_deref(), Some("transport"));
    }
}
