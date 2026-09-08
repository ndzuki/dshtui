//! subagents/* endpoint wrappers (REQ-007 V0.4; official read 0.1.2-rc.1
//! from `@deepseek-ai/dsh-subagent/typert.remote-client`).
//!
//! Wire knowledge (centralized here per Notes/03):
//! - `subagents/list(parentSessionId)` — direct children only (`hasChildren`
//!   guides recursion; the tree is expanded per navigation path);
//! - `subagents/prompt(request)` — single-`request`-parameter endpoint
//!   (business fields nested under `{"request":{...}}`, same convention as
//!   session/fork);
//! - `subagents/interruptByParent(childSessionId, parentSessionId, mode)` —
//!   THREE flat positional params (NOT nested under `request`), read from the
//!   typert descriptor (source: json, wire names childSessionId /
//!   parentSessionId / mode: "continuable").

use super::envelope::ClientError;
use super::types::{SubagentCatalog, SubagentPromptRequest, SubagentReceipt};
use super::unary;

/// `subagents/list` — direct children of one parent session.
pub async fn list(
    http: &reqwest::Client,
    base: &str,
    parent_session_id: &str,
) -> Result<SubagentCatalog, ClientError> {
    let value = unary(
        http,
        base,
        "subagents/list",
        serde_json::json!({ "parentSessionId": parent_session_id }),
    )
    .await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("subagents/list 响应形状异常: {e}")))
}

/// `subagents/prompt` — send a message to a continuable subagent. Content parts
/// reuse the session/prompt `PromptContentPart` vocabulary (incl. image).
pub async fn prompt(
    http: &reqwest::Client,
    base: &str,
    request: &SubagentPromptRequest,
) -> Result<SubagentReceipt, ClientError> {
    let req = serde_json::to_value(request)
        .map_err(|e| ClientError::Protocol(format!("subagents/prompt 参数序列化失败: {e}")))?;
    let value = unary(
        http,
        base,
        "subagents/prompt",
        serde_json::json!({ "request": req }),
    )
    .await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("subagents/prompt 响应形状异常: {e}")))
}

/// `subagents/interruptByParent` — stop a running child from its parent.
/// Flat positional args (childSessionId / parentSessionId / mode:"continuable");
/// an idle/one-shot child is an accepted no-op (receipt `{accepted:true}`).
pub async fn interrupt_by_parent(
    http: &reqwest::Client,
    base: &str,
    child_session_id: &str,
    parent_session_id: &str,
) -> Result<SubagentReceipt, ClientError> {
    let value = unary(
        http,
        base,
        "subagents/interruptByParent",
        serde_json::json!({
            "childSessionId": child_session_id,
            "parentSessionId": parent_session_id,
            "mode": "continuable",
        }),
    )
    .await?;
    serde_json::from_value(value).map_err(|e| {
        ClientError::Protocol(format!("subagents/interruptByParent 响应形状异常: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{PromptContentPart, SessionAddress};

    #[test]
    fn subagent_address_serializes_camel_case_kind() {
        let addr = SessionAddress::subagent("parent-1", "child-1", "continuable");
        let v = serde_json::to_value(&addr).unwrap();
        assert_eq!(v["kind"], "subagent");
        assert_eq!(v["parentSessionId"], "parent-1");
        assert_eq!(v["childSessionId"], "child-1");
        assert_eq!(v["mode"], "continuable");
        assert!(v.get("sessionId").is_none(), "subagent 不带 sessionId");
    }

    #[test]
    fn subagent_catalog_parses_child_and_diagnostic_entries() {
        let v = serde_json::json!({
            "entries": [
                {"kind": "child", "id": "c1", "activity": "running",
                 "hasChildren": true, "mode": "continuable", "label": "走查"},
                {"kind": "child", "id": "c2", "activity": "inactive",
                 "hasChildren": false, "mode": "one-shot"},
                {"kind": "diagnostic", "id": "c3", "reason": "corrupt"}
            ],
            "parentAvailable": true
        });
        let cat: SubagentCatalog = serde_json::from_value(v).unwrap();
        assert_eq!(cat.entries.len(), 3);
        assert!(cat.parent_available);
        match &cat.entries[0] {
            crate::api::types::SubagentListEntry::Child {
                id,
                activity,
                has_children,
                mode,
                label,
            } => {
                assert_eq!(id, "c1");
                assert_eq!(activity, "running");
                assert!(has_children);
                assert_eq!(mode.as_deref(), Some("continuable"));
                assert_eq!(label.as_deref(), Some("走查"));
            }
            other => panic!("expected child, got {other:?}"),
        }
        assert!(matches!(
            &cat.entries[2],
            crate::api::types::SubagentListEntry::Diagnostic { id, reason }
                if id == "c3" && reason == "corrupt"
        ));
    }

    #[test]
    fn subagent_prompt_request_serializes_content_and_identity() {
        let req = SubagentPromptRequest {
            request_id: "r-1".into(),
            parent_session_id: "p-1".into(),
            child_session_id: "c-1".into(),
            mode: "continuable".into(),
            content: vec![PromptContentPart::Text {
                text: "继续".into(),
            }],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["requestId"], "r-1");
        assert_eq!(v["parentSessionId"], "p-1");
        assert_eq!(v["childSessionId"], "c-1");
        assert_eq!(v["mode"], "continuable");
        assert_eq!(v["content"][0]["type"], "text");
    }
}
