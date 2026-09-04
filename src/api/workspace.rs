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

pub async fn open_follow(mux: &Mux) -> Result<StreamHandle, ClientError> {
    mux.open_stream("workspace/follow", serde_json::json!({}))
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
