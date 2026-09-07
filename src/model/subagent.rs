//! Subagent view state (REQ-007 FR-007-01; pure model, no IO).
//!
//! `subagents/list` only returns DIRECT children (`hasChildren` guides
//! recursion), so the tree is expanded lazily along the user's navigation
//! path — never the whole depth-first tree (breadth bound: each level of the
//! navigation path holds one catalog fetch).

use std::collections::BTreeMap;

use crate::api::types::SubagentCatalog;

/// One expanded subagent node in the view.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentNode {
    /// Direct child session id (`childSessionId`).
    pub id: String,
    pub activity: String,
    pub has_children: bool,
    /// "one-shot" | "continuable" (None when the server omits it).
    pub mode: Option<String>,
    pub label: Option<String>,
    /// Children fetched on demand (`subagents/list(childId)`) when the user
    /// opens this node; `None` = not expanded yet.
    pub children: Option<Vec<SubagentNode>>,
}

/// Subagent panel state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubagentViewState {
    /// Panel visibility (open via `:` subagent action).
    pub visible: bool,
    /// Catalog for the CURRENT parent (the session the user opened the panel
    /// from); re-fetched when the bound parent changes.
    pub parent_session_id: Option<String>,
    pub parent_available: bool,
    pub roots: Vec<SubagentNode>,
    /// Breadcrumb of expanded nodes on the navigation path (session ids).
    pub nav_path: Vec<String>,
    /// Cursor row (flattened display index).
    pub selected: usize,
    /// Confirm-pending action (interrupt target id).
    pub interrupt_target: Option<String>,
    pub last_error_code: Option<String>,
    /// Whether the panel is mid-flight on the current parent (generation
    /// guard — stale responses dropped).
    pub loading: bool,
}

impl SubagentViewState {
    pub fn open(&mut self, parent_session_id: &str) {
        self.visible = true;
        self.parent_session_id = Some(parent_session_id.to_string());
        self.roots = Vec::new();
        self.nav_path = Vec::new();
        self.selected = 0;
        self.loading = true;
        self.last_error_code = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// Replace the catalog of the current parent / a navigation child.
    /// `children_of` is the session id whose children this catalog serves
    /// (== the current parent when on the root level).
    pub fn set_catalog(&mut self, children_of: &str, cat: SubagentCatalog) {
        self.loading = false;
        self.parent_available = cat.parent_available;
        let nodes = cat
            .entries
            .into_iter()
            .filter_map(|e| match e {
                crate::api::types::SubagentListEntry::Child {
                    id,
                    activity,
                    has_children,
                    mode,
                    label,
                } => Some(SubagentNode {
                    id,
                    activity,
                    has_children,
                    mode,
                    label,
                    children: None,
                }),
                // diagnostic rows carry no actionable id for this panel level.
                crate::api::types::SubagentListEntry::Diagnostic { .. } => None,
            })
            .collect::<Vec<_>>();
        if self.parent_session_id.as_deref() == Some(children_of) && self.nav_path.is_empty() {
            self.roots = nodes;
        } else if let Some(level) = self.nav_path.iter().position(|p| p == children_of) {
            // nav_path = expanded child ids from the root down. Attach the new
            // catalog at nav_path[level]: walk down levels [0, level).
            let mut container: &mut Vec<SubagentNode> = &mut self.roots;
            for step in self.nav_path.iter().take(level) {
                let Some(pos) = container.iter().position(|n| &n.id == step) else {
                    return;
                };
                let Some(children) = container[pos].children.as_mut() else {
                    // 未展开的层无子列表：不做半成品写入。
                    return;
                };
                container = children;
            }
            if let Some(pos) = container.iter().position(|n| n.id == children_of) {
                container[pos].children = Some(nodes);
            }
        }
    }

    /// Depth-first flattened rows (for list rendering & cursor navigation).
    pub fn flatten(&self) -> Vec<SubagentNodeRef<'_>> {
        fn walk<'a>(nodes: &'a [SubagentNode], depth: usize, out: &mut Vec<SubagentNodeRef<'a>>) {
            for n in nodes {
                out.push(SubagentNodeRef { node: n, depth });
                if let Some(children) = &n.children {
                    walk(children, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.roots, 0, &mut out);
        out
    }
}

/// Borrow of one flattened row with its depth (rendering indent).
#[derive(Debug, Clone, Copy)]
pub struct SubagentNodeRef<'a> {
    pub node: &'a SubagentNode,
    pub depth: usize,
}

/// Lineage breadcrumb — derived only from real data (`SessionMeta.parent_id`
/// chains / subagent address fields); never fabricated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineageCrumb {
    pub session_id: String,
    pub title: Option<String>,
    pub kind: String, // "session" | "subagent"
}

/// Assemble a breadcrumb trail from a leaf back to the root using the known
/// parent map. `parents` maps session id → (title, parent_id or None).
pub fn lineage_breadcrumbs(
    leaf: &str,
    parents: &BTreeMap<String, (Option<String>, Option<String>)>,
) -> Vec<LineageCrumb> {
    let mut out = Vec::new();
    let mut cur = leaf.to_string();
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 64 {
            break; // 防环兜底（异常数据）
        }
        let Some((title, parent)) = parents.get(&cur) else {
            out.push(LineageCrumb {
                session_id: cur,
                title: None,
                kind: "session".into(),
            });
            break;
        };
        out.push(LineageCrumb {
            session_id: cur.clone(),
            title: title.clone(),
            kind: "session".into(),
        });
        match parent {
            Some(p) if !p.is_empty() && p != &cur => cur = p.clone(),
            _ => break,
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SubagentListEntry;

    fn child(
        id: &str,
        activity: &str,
        has_children: bool,
        mode: Option<&str>,
    ) -> SubagentListEntry {
        SubagentListEntry::Child {
            id: id.into(),
            activity: activity.into(),
            has_children,
            mode: mode.map(String::from),
            label: None,
        }
    }

    fn catalog(entries: Vec<SubagentListEntry>) -> SubagentCatalog {
        SubagentCatalog {
            entries,
            parent_available: true,
        }
    }

    #[test]
    fn open_sets_parent_and_loading() {
        let mut s = SubagentViewState::default();
        s.open("parent-1");
        assert!(s.visible);
        assert_eq!(s.parent_session_id.as_deref(), Some("parent-1"));
        assert!(s.loading);
    }

    #[test]
    fn set_catalog_roots_at_parent_level() {
        let mut s = SubagentViewState::default();
        s.open("p1");
        s.set_catalog(
            "p1",
            catalog(vec![child("c1", "running", true, Some("continuable"))]),
        );
        assert!(!s.loading);
        assert_eq!(s.roots.len(), 1);
        assert_eq!(s.roots[0].id, "c1");
        assert_eq!(s.flatten().len(), 1);
    }

    #[test]
    fn set_catalog_attaches_to_nav_child_and_drops_diagnostics() {
        let mut s = SubagentViewState::default();
        s.open("p1");
        s.set_catalog(
            "p1",
            catalog(vec![
                child("c1", "running", true, Some("continuable")),
                SubagentListEntry::Diagnostic {
                    id: "c9".into(),
                    reason: "corrupt".into(),
                },
            ]),
        );
        s.nav_path = vec!["c1".into()];
        s.set_catalog("c1", catalog(vec![child("gc1", "inactive", false, None)]));
        let rows = s.flatten();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].node.id, "c1");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].node.id, "gc1");
    }

    #[test]
    fn lineage_breadcrumbs_reverse_chain_no_fabrication() {
        let mut parents = BTreeMap::new();
        parents.insert("leaf".into(), (Some("叶子".into()), Some("mid".into())));
        parents.insert("mid".into(), (Some("中间".into()), Some("root".into())));
        parents.insert("root".into(), (Some("根".into()), None));
        let crumbs = lineage_breadcrumbs("leaf", &parents);
        let ids: Vec<&str> = crumbs.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, vec!["root", "mid", "leaf"]);
        assert_eq!(crumbs[0].title.as_deref(), Some("根"));
        // Unknown leaf: single crumb with no title, never invented.
        let crumbs = lineage_breadcrumbs("ghost", &parents);
        assert_eq!(crumbs.len(), 1);
        assert_eq!(crumbs[0].title, None);
    }

    #[test]
    fn lineage_guard_breaks_cycles() {
        let mut parents = BTreeMap::new();
        parents.insert("a".into(), (None, Some("b".into())));
        parents.insert("b".into(), (None, Some("a".into())));
        let crumbs = lineage_breadcrumbs("a", &parents);
        assert!(crumbs.len() <= 65, "防环有界，得到 {}", crumbs.len());
    }
}
