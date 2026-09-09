//! Workspace and session metadata store (Notes/06 §1: lightweight metadata
//! ~500B/item; full projections are not kept).

use std::collections::HashMap;

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32String};

use crate::api::types::{SessionId, SessionMeta, WorkspaceId};

/// Project (workspace) row: id/title/member session ids.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceMeta {
    pub id: WorkspaceId,
    pub title: Option<String>,
    pub session_ids: Vec<SessionId>,
}

// ---------- REQ-006 Sidebar 视图态（FR-006-02 视图半，D-034：gv 仅本地
// 视图态、无远端写；AC-006-03/11） ----------

/// 侧栏分组视图（`gv` 切换；仅本地态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupBy {
    /// workspace/follow 提供的项目分组（无 workspace 对象时退化为平铺）。
    #[default]
    Workspace,
    /// 全部会话平铺（按 updated desc）。
    Flat,
}

impl GroupBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            GroupBy::Workspace => "workspace",
            GroupBy::Flat => "flat",
        }
    }
}

/// 侧栏排序视图（`gv` 切换；workspace 分组内的会话顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrderBy {
    /// 按 workspace.session_ids（用户手工顺序）。
    Manual,
    /// 按 updated_at_ms 降序。
    #[default]
    Updated,
}

impl OrderBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderBy::Manual => "manual",
            OrderBy::Updated => "updated",
        }
    }
}

/// 侧栏本地视图状态（REQ-006 §5 `WorkspaceViewState`；仅内存，D-034 无远端
/// 写）。`collapsed` 保持既有 h/l 语义（默认展开、h 折叠），从 AppState 的
/// `collapsed_workspaces` 迁入。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceViewState {
    pub group_by: GroupBy,
    pub order_by: OrderBy,
    /// 折叠的 workspace（▸）；展开为默认（▾）。
    pub collapsed: std::collections::HashSet<WorkspaceId>,
}

impl WorkspaceViewState {
    pub fn collapse_all(&mut self, store: &WorkspaceStore) {
        for w in &store.workspaces {
            self.collapsed.insert(w.id.clone());
        }
    }

    pub fn expand_all(&mut self) {
        self.collapsed.clear();
    }

    pub fn toggle(&mut self, id: &WorkspaceId) {
        if !self.collapsed.remove(id) {
            self.collapsed.insert(id.clone());
        }
    }

    pub fn is_collapsed(&self, id: &WorkspaceId) -> bool {
        self.collapsed.contains(id)
    }

    /// `gv` 单键循环 group_by × order_by 四组合（AC-006-03：每按一次视图
    /// 即时变化；仅本地态）。每次只动一轴，行为可预期：先切 order，再切
    /// group：workspace/updated → workspace/manual → flat/manual →
    /// flat/updated → 回到起点。
    pub fn cycle(&mut self) {
        use GroupBy::*;
        use OrderBy::*;
        (self.group_by, self.order_by) = match (self.group_by, self.order_by) {
            (Workspace, Updated) => (Workspace, Manual),
            (Workspace, Manual) => (Flat, Manual),
            (Flat, Manual) => (Flat, Updated),
            (Flat, Updated) => (Workspace, Updated),
        };
    }
}

/// 侧栏一行（渲染与光标移动/打开的共享行模型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarRow {
    /// workspace header（可折叠）。
    WorkspaceHeader { id: WorkspaceId, collapsed: bool },
    /// 会话行（元数据按 id 从 store 查询）。
    Session(SessionId),
}

impl SidebarRow {
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            SidebarRow::Session(id) => Some(id),
            _ => None,
        }
    }

    pub fn workspace_id(&self) -> Option<&WorkspaceId> {
        match self {
            SidebarRow::WorkspaceHeader { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// 按当前视图态计算侧栏可见行（纯函数；渲染与 reducer 共享同一 seam）。
///
/// - `GroupBy::Workspace`：每个 workspace 一个 header（折叠时无子行）+
///   组内会话按 `OrderBy`（manual=session_ids 顺序 / updated=updated_at_ms
///   降序）；不在任何 workspace 的会话按 updated desc 追加尾部。
/// - `GroupBy::Flat`：全部会话平铺（updated desc；order 只作用于分组内）。
/// - store 中已不存在的 session id 会被跳过（列表不漂移）。
pub fn sidebar_rows(view: &WorkspaceViewState, store: &WorkspaceStore) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    match view.group_by {
        GroupBy::Flat => {
            for meta in store.sessions_sorted() {
                rows.push(SidebarRow::Session(meta.id.clone()));
            }
        }
        GroupBy::Workspace => {
            if store.workspaces.is_empty() {
                // 无 workspace 对象（follow 未到达）→ 退化为平铺。
                for meta in store.sessions_sorted() {
                    rows.push(SidebarRow::Session(meta.id.clone()));
                }
                return rows;
            }
            let mut grouped: Vec<SessionId> = Vec::new();
            for workspace in &store.workspaces {
                rows.push(SidebarRow::WorkspaceHeader {
                    id: workspace.id.clone(),
                    collapsed: view.is_collapsed(&workspace.id),
                });
                // 折叠：会话仍标记为已分组（避免被当未分组在尾部渲染），但
                // 不产生可见行。
                if view.is_collapsed(&workspace.id) {
                    for sid in &workspace.session_ids {
                        if store.sessions.contains_key(sid) {
                            grouped.push(sid.clone());
                        }
                    }
                    continue;
                }
                let mut members: Vec<&SessionId> = workspace
                    .session_ids
                    .iter()
                    .filter(|sid| store.sessions.contains_key(*sid))
                    .collect();
                match view.order_by {
                    OrderBy::Manual => {}
                    OrderBy::Updated => members.sort_by_key(|sid| {
                        std::cmp::Reverse(
                            store
                                .sessions
                                .get(*sid)
                                .map(|m| m.updated_at_ms)
                                .unwrap_or(0),
                        )
                    }),
                }
                for sid in members {
                    grouped.push((*sid).clone());
                    rows.push(SidebarRow::Session((*sid).clone()));
                }
            }
            // 未分组的会话尾部平铺（updated desc）。
            for meta in store.sessions_sorted() {
                if !grouped.contains(&meta.id) {
                    rows.push(SidebarRow::Session(meta.id.clone()));
                }
            }
        }
    }
    rows
}

/// Sidebar data source: workspace/follow grouping + session/list session rows.
#[derive(Debug, Default)]
pub struct WorkspaceStore {
    pub workspaces: Vec<WorkspaceMeta>,
    pub sessions: HashMap<SessionId, SessionMeta>,
    /// session/list pagination cursor (None=end reached).
    pub next_cursor: Option<String>,
}

impl WorkspaceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Incremental session upsert (list pagination merges without rebuilding
    /// the whole table, Notes/06 §1).
    /// REQ §5 memory budget: beyond 20000 sessions keep only the most recent
    /// 5000 (grouping summaries live in workspace rows and are not affected).
    pub fn upsert_session(&mut self, meta: SessionMeta) {
        const CAP: usize = 20_000;
        const KEEP: usize = 5_000;
        self.sessions.insert(meta.id.clone(), meta);
        if self.sessions.len() > CAP {
            let mut by_updated: Vec<SessionId> =
                self.sessions.values().map(|m| m.id.clone()).collect();
            by_updated.sort_by_key(|id| {
                std::cmp::Reverse(self.sessions.get(id).map(|m| m.updated_at_ms).unwrap_or(0))
            });
            for stale in by_updated.into_iter().skip(KEEP) {
                self.sessions.remove(&stale);
            }
        }
    }

    /// Find the group by workspace id and append the session id (unknown
    /// workspace → ignore, wait for workspace/follow).
    pub fn attach_session_to_workspace(&mut self, ws: &WorkspaceId, sid: &SessionId) {
        if let Some(w) = self.workspaces.iter_mut().find(|w| &w.id == ws) {
            if !w.session_ids.contains(sid) {
                w.session_ids.push(sid.clone());
            }
        }
    }

    /// Incremental workspace/follow merge (unknown shapes are tolerated by the
    /// api layer before the raw id/title reaches here).
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

    /// Clear and rebuild (workspace/follow snapshot after reconnect).
    pub fn clear_workspaces(&mut self) {
        self.workspaces.clear();
    }

    /// Session count (sidebar statistic; reads the store size directly, not an
    /// official projection).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Iterate sessions for the picker (updated_at_ms descending).
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
                // id/workspace 为私有 newtype 字段，需先经 get() 取 owned 值再借；
                // 其余 Option<String> 字段仍直接 borrow meta（无额外拷贝）。
                let id = meta.id.get();
                let ws = meta.workspace.as_ref().map(|w| w.get());
                let fields = [
                    id.as_str(),
                    meta.title.as_deref().unwrap_or(""),
                    meta.cwd.as_deref().unwrap_or(""),
                    ws.as_deref().unwrap_or(""),
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
            id: SessionId::new(id.into()),
            title: Some(format!("t-{id}")),
            cwd: None,
            updated_at_ms: updated,
            running: false,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: ws.map(|s| WorkspaceId::new(s.into())),
            last_turn_preview: None,
        }
    }

    #[test]
    fn upsert_merges_pages() {
        let mut store = WorkspaceStore::new();
        store.upsert_session(meta("a", 1, None));
        store.upsert_session(meta("b", 2, None));
        // Page 2 contains an updated version of a → no duplicate, take the new
        // value.
        store.upsert_session(meta("a", 9, None));
        store.upsert_session(meta("c", 3, None));
        assert_eq!(store.session_count(), 3);
        assert_eq!(store.sessions[&SessionId::new("a".into())].updated_at_ms, 9);
    }

    #[test]
    fn sessions_sorted_by_updated_desc() {
        let mut store = WorkspaceStore::new();
        store.upsert_session(meta("old", 1, None));
        store.upsert_session(meta("new", 100, None));
        store.upsert_session(meta("mid", 50, None));
        let sorted = store.sessions_sorted();
        assert_eq!(sorted[0].id, SessionId::new("new".into()));
        assert_eq!(sorted[2].id, SessionId::new("old".into()));
    }

    #[test]
    fn workspace_attach_and_upsert() {
        let mut store = WorkspaceStore::new();
        store.upsert_workspace(WorkspaceId::new("ws1".into()), Some("项目A".into()));
        store.upsert_session(meta("s1", 1, Some("ws1")));
        store.attach_session_to_workspace(
            &WorkspaceId::new("ws1".into()),
            &SessionId::new("s1".into()),
        );
        // Duplicate attach produces no duplicate members.
        store.attach_session_to_workspace(
            &WorkspaceId::new("ws1".into()),
            &SessionId::new("s1".into()),
        );
        assert_eq!(store.workspaces.len(), 1);
        assert_eq!(store.workspaces[0].session_ids.len(), 1);
        // Attach to an unknown workspace is safely ignored.
        store.attach_session_to_workspace(
            &WorkspaceId::new("nope".into()),
            &SessionId::new("s1".into()),
        );
        // Reconnect: clear, then rebuild.
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
        let ids: Vec<String> = matched.iter().map(|m| m.id.get()).collect();
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
        assert!(!matched.iter().any(|m| m.id == SessionId::new("s-1".into())));
        assert!(matched.len() >= 2, "s-2 preview and s-3 cwd both match");

        // Specific query only matches the s-3 cwd token (deterministic single hit).
        let specific = store.match_sessions("deploy-target");
        assert_eq!(specific.len(), 1);
        assert_eq!(specific[0].id, SessionId::new("s-3".into()));
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

    #[test]
    fn session_list_over_20000_keeps_recent_5000() {
        // REQ §5: N > 20000 keeps only the most recent 5000.
        let mut store = WorkspaceStore::new();
        for i in 0..20_001 {
            store.upsert_session(meta(&format!("s{i:05}"), i as i64, None));
        }
        assert_eq!(store.session_count(), 5_000);
        // The newest (highest updated_at_ms) survives, the oldest does not.
        assert!(store
            .sessions
            .contains_key(&SessionId::new("s20000".into())));
        assert!(!store
            .sessions
            .contains_key(&SessionId::new("s00000".into())));
    }

    // ---------- REQ-006 视图态（FR-006-02 / D-034） ----------

    fn ws(store: &mut WorkspaceStore, id: &str) -> WorkspaceId {
        let wid = WorkspaceId::new(id.into());
        store.upsert_workspace(wid.clone(), Some(format!("项目{id}")));
        wid
    }

    fn row_ids(rows: &[SidebarRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                SidebarRow::WorkspaceHeader { id, .. } => format!("[{id}]"),
                SidebarRow::Session(id) => id.get(),
            })
            .collect()
    }

    #[test]
    fn sidebar_view_state_gv_cycles_group_and_order_ac006_03() {
        let mut view = WorkspaceViewState::default();
        assert_eq!(view.group_by, GroupBy::Workspace);
        assert_eq!(view.order_by, OrderBy::Updated);
        // 每按一次 gv 只动一轴：order → group → order → group 回起点。
        view.cycle();
        assert_eq!(view.group_by, GroupBy::Workspace);
        assert_eq!(view.order_by, OrderBy::Manual);
        view.cycle();
        assert_eq!(view.group_by, GroupBy::Flat);
        assert_eq!(view.order_by, OrderBy::Manual);
        view.cycle();
        assert_eq!(view.group_by, GroupBy::Flat);
        assert_eq!(view.order_by, OrderBy::Updated);
        view.cycle();
        assert_eq!(view.group_by, GroupBy::Workspace);
        assert_eq!(view.order_by, OrderBy::Updated, "四态循环回到起点");
    }

    #[test]
    fn sidebar_rows_grouped_manual_order_and_collapse() {
        let mut store = WorkspaceStore::new();
        let ws1 = ws(&mut store, "ws1");
        store.upsert_session(meta("a", 1, None));
        store.upsert_session(meta("b", 200, None));
        store.upsert_session(meta("c", 50, None));
        store.upsert_session(meta("free", 999, None)); // 未分组
        for sid in ["b", "a", "c"] {
            store.attach_session_to_workspace(&ws1, &SessionId::new(sid.into()));
        }
        // manual：按 session_ids 顺序 [b, a, c]。
        let view = WorkspaceViewState {
            group_by: GroupBy::Workspace,
            order_by: OrderBy::Manual,
            collapsed: Default::default(),
        };
        let rows = sidebar_rows(&view, &store);
        assert_eq!(
            row_ids(&rows),
            vec!["[ws1]", "b", "a", "c", "free"],
            "manual=session_ids 顺序；未分组尾部"
        );
        // updated：组内按 updated desc → c(50) 在 a(1) 前？desc → b(200), c(50), a(1)。
        let view = WorkspaceViewState {
            group_by: GroupBy::Workspace,
            order_by: OrderBy::Updated,
            collapsed: Default::default(),
        };
        let rows = sidebar_rows(&view, &store);
        assert_eq!(
            row_ids(&rows),
            vec!["[ws1]", "b", "c", "a", "free"],
            "updated 组内降序"
        );
        // 折叠：只留 header。
        let mut view = view;
        view.collapsed.insert(ws1.clone());
        let rows = sidebar_rows(&view, &store);
        assert_eq!(
            row_ids(&rows),
            vec!["[ws1]", "free"],
            "折叠 workspace 隐藏子行（未分组仍显示）"
        );
        // toggle 展开。
        view.toggle(&ws1);
        assert!(!view.is_collapsed(&ws1));
    }

    #[test]
    fn sidebar_rows_flat_lists_all_sessions_sorted_ac006_03() {
        let mut store = WorkspaceStore::new();
        let ws1 = ws(&mut store, "ws1");
        store.upsert_session(meta("old", 1, None));
        store.upsert_session(meta("new", 100, None));
        store.attach_session_to_workspace(&ws1, &SessionId::new("old".into()));
        let view = WorkspaceViewState {
            group_by: GroupBy::Flat,
            order_by: OrderBy::Updated,
            collapsed: Default::default(),
        };
        let rows = sidebar_rows(&view, &store);
        assert_eq!(
            row_ids(&rows),
            vec!["new", "old"],
            "flat 全量平铺（updated desc），无 header"
        );
        // 无 workspace 对象（follow 未达）时 workspace 分组退化为平铺。
        let empty = WorkspaceStore::new();
        let view = WorkspaceViewState::default();
        let rows = sidebar_rows(&view, &empty);
        assert!(rows.is_empty());
    }
}
