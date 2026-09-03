//! 工作区与会话元数据存储（Notes/06 §1：轻量元数据 ~500B/条，不保留完整 projections）。

use std::collections::HashMap;

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32String};

use crate::api::types::{SessionId, SessionMeta, WorkspaceId};

/// 项目（workspace）行：id/title/成员会话 id。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceMeta {
    pub id: WorkspaceId,
    pub title: Option<String>,
    pub session_ids: Vec<SessionId>,
}

/// 侧栏数据源：workspace/follow 分组 + session/list 会话行。
#[derive(Debug, Default)]
pub struct WorkspaceStore {
    pub workspaces: Vec<WorkspaceMeta>,
    pub sessions: HashMap<SessionId, SessionMeta>,
    /// session/list 分页游标（None=已到末尾）。
    pub next_cursor: Option<String>,
}

impl WorkspaceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 增量 upsert 会话（list 分页合并，不整表重建，Notes/06 §1）。
    pub fn upsert_session(&mut self, meta: SessionMeta) {
        self.sessions.insert(meta.id.clone(), meta);
    }

    /// 按 workspace id 找到分组并追加会话 id（不存在则忽略，等待 workspace/follow）。
    pub fn attach_session_to_workspace(&mut self, ws: &WorkspaceId, sid: &SessionId) {
        if let Some(w) = self.workspaces.iter_mut().find(|w| &w.id == ws) {
            if !w.session_ids.contains(sid) {
                w.session_ids.push(sid.clone());
            }
        }
    }

    /// workspace/follow 增量合并（未知形态由 api 层容忍后传入原始 id/标题）。
    pub fn upsert_workspace(&mut self, id: WorkspaceId, title: Option<String>) {
        match self.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                if title.is_some() {
                    w.title = title;
                }
            }
            None => self.workspaces.push(WorkspaceMeta {
                id,
                title,
                session_ids: Vec::new(),
            }),
        }
    }

    /// 清空重建（重连后 workspace/follow snapshot）。
    pub fn clear_workspaces(&mut self) {
        self.workspaces.clear();
    }

    /// 会话总数（侧栏统计，直接读 store 大小，非官方投影场景）。
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// 供 picker 的会话迭代（按 updated_at_ms 降序）。
    pub fn sessions_sorted(&self) -> Vec<&SessionMeta> {
        let mut v: Vec<&SessionMeta> = self.sessions.values().collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.updated_at_ms));
        v
    }

    /// Fuzzy-match sessions for the picker (ADR-003: nucleo, no external fzf).
    /// Fields: id / title / cwd / workspace id / last turn preview. The result
    /// keeps the updated-descending order as tie-break; an empty query returns
    /// the full sorted list. Per-keystroke cost stays bounded (V0.1 list size).
    pub fn match_sessions(&self, query: &str) -> Vec<&SessionMeta> {
        let sorted = self.sessions_sorted();
        let query = query.trim();
        if query.is_empty() {
            return sorted;
        }
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::default();
        let mut scored: Vec<(u32, &SessionMeta)> = sorted
            .into_iter()
            .filter_map(|meta| {
                let fields = [
                    meta.id.0.as_str(),
                    meta.title.as_deref().unwrap_or(""),
                    meta.cwd.as_deref().unwrap_or(""),
                    meta.workspace.as_ref().map(|w| w.0.as_str()).unwrap_or(""),
                    meta.last_turn_preview.as_deref().unwrap_or(""),
                ];
                let best = fields
                    .iter()
                    .filter_map(|field| {
                        if field.is_empty() {
                            return None;
                        }
                        let haystack = Utf32String::from(*field);
                        pattern.score(haystack.slice(..), &mut matcher)
                    })
                    .max()?;
                Some((best, meta))
            })
            .collect();
        // `sort_by_key` is stable: equal scores keep updated-descending order.
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        scored.into_iter().map(|(_, meta)| meta).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, updated: i64, ws: Option<&str>) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.into()),
            title: Some(format!("t-{id}")),
            cwd: None,
            updated_at_ms: updated,
            running: false,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: ws.map(|s| WorkspaceId(s.into())),
            last_turn_preview: None,
        }
    }

    #[test]
    fn upsert_merges_pages() {
        let mut store = WorkspaceStore::new();
        store.upsert_session(meta("a", 1, None));
        store.upsert_session(meta("b", 2, None));
        // 分页第二页包含 a 的更新版本 → 不重复、取新值。
        store.upsert_session(meta("a", 9, None));
        store.upsert_session(meta("c", 3, None));
        assert_eq!(store.session_count(), 3);
        assert_eq!(store.sessions[&SessionId("a".into())].updated_at_ms, 9);
    }

    #[test]
    fn sessions_sorted_by_updated_desc() {
        let mut store = WorkspaceStore::new();
        store.upsert_session(meta("old", 1, None));
        store.upsert_session(meta("new", 100, None));
        store.upsert_session(meta("mid", 50, None));
        let sorted = store.sessions_sorted();
        assert_eq!(sorted[0].id, SessionId("new".into()));
        assert_eq!(sorted[2].id, SessionId("old".into()));
    }

    #[test]
    fn workspace_attach_and_upsert() {
        let mut store = WorkspaceStore::new();
        store.upsert_workspace(WorkspaceId("ws1".into()), Some("项目A".into()));
        store.upsert_session(meta("s1", 1, Some("ws1")));
        store.attach_session_to_workspace(&WorkspaceId("ws1".into()), &SessionId("s1".into()));
        // 重复 attach 不产生重复成员。
        store.attach_session_to_workspace(&WorkspaceId("ws1".into()), &SessionId("s1".into()));
        assert_eq!(store.workspaces.len(), 1);
        assert_eq!(store.workspaces[0].session_ids.len(), 1);
        // 未知 workspace attach 安全忽略。
        store.attach_session_to_workspace(&WorkspaceId("nope".into()), &SessionId("s1".into()));
        // 重连清空后重建。
        store.clear_workspaces();
        assert!(store.workspaces.is_empty());
        assert_eq!(store.session_count(), 1, "清空 workspace 不影响会话");
    }

    #[test]
    fn match_sessions_empty_query_returns_updated_desc_order() {
        let mut store = WorkspaceStore::new();
        store.upsert_session(meta("old", 1, None));
        store.upsert_session(meta("new", 100, None));
        store.upsert_session(meta("mid", 50, None));
        let matched = store.match_sessions("  ");
        let ids: Vec<&str> = matched.iter().map(|m| m.id.0.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old"]);
    }

    #[test]
    fn match_sessions_fuzzy_ranks_across_fields_and_keeps_order_ties() {
        let mut store = WorkspaceStore::new();
        store.upsert_session(meta("s-1", 100, None));
        store.upsert_session(meta("s-2", 90, None));
        let mut cwd_match = meta("s-3", 80, None);
        cwd_match.cwd = Some("/home/nd/src/deploy-target".into());
        store.upsert_session(cwd_match);
        // A partial preview hit exists on s-2 for the broad query.
        let mut preview_match = meta("s-2", 90, None);
        preview_match.last_turn_preview = Some("deploy 到生产".into());
        store.upsert_session(preview_match);

        // Broad query hits several fields; every hit must stay, non-hits go.
        let matched = store.match_sessions("deploy");
        assert!(!matched.is_empty());
        assert!(!matched.iter().any(|m| m.id == SessionId("s-1".into())));
        assert!(matched.len() >= 2, "s-2 preview and s-3 cwd both match");

        // Specific query only matches the s-3 cwd token (deterministic single hit).
        let specific = store.match_sessions("deploy-target");
        assert_eq!(specific.len(), 1);
        assert_eq!(specific[0].id, SessionId("s-3".into()));
    }

    #[test]
    fn picker_match_1042_sessions_is_bounded() {
        // AC-001-03: picker search < 30ms on 1042 local sessions.
        let mut store = WorkspaceStore::new();
        for i in 0..1042 {
            let mut m = meta(&format!("sess-{i:04}"), i as i64, None);
            m.title = Some(format!("Session {i} deploy build"));
            m.cwd = Some(format!("/home/nd/projects/p{}", i % 40));
            m.last_turn_preview = Some(format!("turn preview {i}"));
            store.upsert_session(m);
        }
        let start = std::time::Instant::now();
        let matched = store.match_sessions("depl");
        let elapsed = start.elapsed();
        assert!(!matched.is_empty());
        assert!(
            elapsed.as_millis() < 30,
            "picker match too slow: {} ms",
            elapsed.as_millis()
        );
        eprintln!("picker_match_1042: {} ms", elapsed.as_millis());
    }
}
