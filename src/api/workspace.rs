//! workspace/* endpoint wrappers (V0.1 subset: follow, Notes/03 §3).
//!
//! The frame shape of workspace/follow is not recorded field-by-field in
//! Notes/03, so this layer parses tolerantly: known shapes
//! (`{type:"snapshot", workspaces:[...]}` / `{workspaces:[...]}` / a flat
//! list) are parsed directly; unknown shapes keep the raw frame and log it to
//! tracing (never silently dropped).
//!
//! Field-level knowledge of the frame shape lives HERE — app/ui must not parse
//! workspace frame fields by hand (PROJECT-CONVENTIONS: protocol knowledge is
//! centralized in the api layer).

use serde_json::Value;

use super::envelope::ClientError;
use super::mux::{Mux, StreamHandle};
use super::unary;

pub async fn open_follow(mux: &Mux) -> Result<StreamHandle, ClientError> {
    mux.open_stream("workspace/follow", serde_json::json!({}))
        .await
}

// ---------- REQ-006 workspace mutation endpoints (V0.3) ----------
//
// All workspace mutations nest their business fields under `{"request":{...}}`
// (official read 0.1.2-rc.1 from dsh-api-workspace-controller
// typert.remote-client.js). `archiveSession` lives in the **workspace**
// namespace and takes only `sessionId` (FR-006-02 fact correction).

/// `workspace/create` — adopt an existing directory as a workspace.
pub async fn create_workspace(
    http: &reqwest::Client,
    base: &str,
    path: &str,
) -> Result<serde_json::Value, ClientError> {
    unary(
        http,
        base,
        "workspace/create",
        serde_json::json!({ "request": { "path": path } }),
    )
    .await
}

/// `workspace/rename` — retitle a workspace.
pub async fn rename_workspace(
    http: &reqwest::Client,
    base: &str,
    workspace_id: &str,
    title: &str,
) -> Result<serde_json::Value, ClientError> {
    unary(
        http,
        base,
        "workspace/rename",
        serde_json::json!({ "request": { "workspaceId": workspace_id, "title": title } }),
    )
    .await
}

/// `workspace/delete` — delete a workspace registration.
pub async fn delete_workspace(
    http: &reqwest::Client,
    base: &str,
    workspace_id: &str,
) -> Result<serde_json::Value, ClientError> {
    unary(
        http,
        base,
        "workspace/delete",
        serde_json::json!({ "request": { "workspaceId": workspace_id } }),
    )
    .await
}

/// `workspace/insertBefore` — DOM-insertBefore-like workspace order mutation.
pub async fn insert_before(
    http: &reqwest::Client,
    base: &str,
    workspace_id: &str,
    before_workspace_id: Option<&str>,
) -> Result<serde_json::Value, ClientError> {
    let mut req = serde_json::json!({ "workspaceId": workspace_id });
    if let Some(before) = before_workspace_id {
        req["beforeWorkspaceId"] = serde_json::json!(before);
    }
    unary(
        http,
        base,
        "workspace/insertBefore",
        serde_json::json!({ "request": req }),
    )
    .await
}

/// `workspace/insertSessionBefore` — move a session within a workspace's
/// manual order.
pub async fn insert_session_before(
    http: &reqwest::Client,
    base: &str,
    workspace_id: &str,
    session_id: &str,
    before_session_id: Option<&str>,
) -> Result<serde_json::Value, ClientError> {
    let mut req = serde_json::json!({
        "workspaceId": workspace_id,
        "sessionId": session_id,
    });
    if let Some(before) = before_session_id {
        req["beforeSessionId"] = serde_json::json!(before);
    }
    unary(
        http,
        base,
        "workspace/insertSessionBefore",
        serde_json::json!({ "request": req }),
    )
    .await
}

/// `workspace/archiveSession` — archive a session (workspace namespace; no
/// workspaceId on the wire, FR-006-02 fact correction).
pub async fn archive_session(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
) -> Result<serde_json::Value, ClientError> {
    unary(
        http,
        base,
        "workspace/archiveSession",
        serde_json::json!({ "request": { "sessionId": session_id } }),
    )
    .await
}

/// One workspace row extracted from a follow frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceItem {
    pub id: String,
    pub title: Option<String>,
    pub session_ids: Vec<String>,
}

/// Parse a follow frame into workspace items (tolerant of the shapes seen so
/// far: `{workspaces:[...]}`, `{items:[...]}`, a bare array, or a single item).
pub fn extract_workspaces(frame: &Value) -> Option<Vec<WorkspaceItem>> {
    let raw_items = frame
        .get("workspaces")
        .or_else(|| frame.get("items"))
        .and_then(|v| v.as_array())
        .or_else(|| frame.as_array())
        .cloned()
        .or_else(|| {
            // A single workspace object is also accepted.
            if frame.get("id").and_then(|v| v.as_str()).is_some() {
                Some(vec![frame.clone()])
            } else {
                None
            }
        });
    let Some(raw_items) = raw_items else {
        tracing::warn!("workspace/follow 帧形态无法识别，保留原始帧");
        return None;
    };
    let items = raw_items
        .into_iter()
        .map(|item| WorkspaceItem {
            id: item
                .get("id")
                .or_else(|| item.get("workspaceId"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            title: item.get("title").and_then(|v| v.as_str()).map(String::from),
            session_ids: item
                .get("sessionIds")
                .or_else(|| item.get("sessions"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .filter(|item| !item.id.is_empty())
        .collect();
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_known_shapes_tolerantly() {
        let snapshot = json!({"type": "snapshot", "workspaces": [
            {"id": "ws-1", "title": "A", "sessionIds": ["s1", "s2"]},
            {"id": "ws-2", "title": "B", "sessions": ["s3"]}
        ]});
        let items = extract_workspaces(&snapshot).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "ws-1");
        assert_eq!(items[0].session_ids, vec!["s1", "s2"]);
        assert_eq!(items[1].id, "ws-2");
        assert_eq!(items[1].session_ids, vec!["s3"]);

        // bare array and single item shapes
        let arr = json!([{"id": "ws-3", "title": "C"}]);
        assert_eq!(
            extract_workspaces(&arr).unwrap()[0].title.as_deref(),
            Some("C")
        );
        let single = json!({"id": "ws-4", "title": "D", "sessionIds": []});
        assert_eq!(extract_workspaces(&single).unwrap()[0].id, "ws-4");
    }

    #[test]
    fn unknown_shape_returns_none_and_keeps_frame() {
        assert!(extract_workspaces(&json!({"future": true})).is_none());
    }
}
