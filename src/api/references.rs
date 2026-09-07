//! @-mention candidate sources (REQ-007 V0.4; official read 0.1.2-rc.1).
//!
//! Wire knowledge (centralized here per Notes/03):
//! - `fileReferences/list(agentId, query)` → file/directory candidates (the
//!   agent is resolved from the session identity; the typert descriptor
//!   carries it as a scope `agentId` — same convention as commands/list);
//! - `sessionReferenceResolver/candidates(agentId, query)` →
//!   `SessionReferenceMentionCandidate[]` (mention text is prebuilt by the
//!   resolver and can be pasted straight back).
//!
//! The official @ candidate set is files + sessions ONLY (no models/slash);
//! slash completion stays with the composer `/` menu / command palette.

use super::envelope::ClientError;
use super::types::{FileReferenceCandidate, SessionReferenceMentionCandidate};
use super::unary;

/// `fileReferences/list` — file/directory path candidates for one agent.
pub async fn file_references(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    query: &str,
) -> Result<Vec<FileReferenceCandidate>, ClientError> {
    let value = unary(
        http,
        base,
        "fileReferences/list",
        serde_json::json!({ "agentId": agent_id, "query": query }),
    )
    .await?;
    parse_array(value, "fileReferences/list")
}

/// `sessionReferenceResolver/candidates` — session mention candidates.
pub async fn session_candidates(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    query: &str,
) -> Result<Vec<SessionReferenceMentionCandidate>, ClientError> {
    let value = unary(
        http,
        base,
        "sessionReferenceResolver/candidates",
        serde_json::json!({ "agentId": agent_id, "query": query }),
    )
    .await?;
    parse_array(value, "sessionReferenceResolver/candidates")
}

/// Tolerant array parser: accept a bare array or `{items:[...]}` /
/// `{candidates:[...]}` wrappers (shape `[未验证]`; contract smoke locks it).
fn parse_array<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    method: &str,
) -> Result<Vec<T>, ClientError> {
    let arr = value
        .as_array()
        .cloned()
        .or_else(|| value.get("items").and_then(|v| v.as_array()).cloned())
        .or_else(|| value.get("candidates").and_then(|v| v.as_array()).cloned())
        .or_else(|| value.get("results").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        match serde_json::from_value::<T>(v) {
            Ok(item) => out.push(item),
            Err(e) => tracing::warn!(error = %e, "{method} 一条候选解析失败，跳过"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reference_candidate_parses_kind() {
        let v: FileReferenceCandidate =
            serde_json::from_value(serde_json::json!({"path": "src/api", "kind": "directory"}))
                .unwrap();
        assert_eq!(v.path, "src/api");
        assert_eq!(v.kind, "directory");
    }

    #[test]
    fn session_candidate_parses_mention_fields() {
        let v: SessionReferenceMentionCandidate = serde_json::from_value(serde_json::json!({
            "sessionId": "s1", "label": "部署排查", "cwd": "/home/nd",
            "sameWorkspace": true, "createdAt": 1000,
            "mention": "@[部署排查](dsh-session:s1)"
        }))
        .unwrap();
        assert_eq!(v.session_id, "s1");
        assert_eq!(v.label, "部署排查");
        assert!(v.same_workspace);
        assert_eq!(v.mention, "@[部署排查](dsh-session:s1)");
    }

    #[test]
    fn array_parser_tolerates_wrappers_and_bad_rows() {
        // bare array
        let v: Vec<FileReferenceCandidate> =
            parse_array(serde_json::json!([{"path": "a.rs", "kind": "file"}]), "t").unwrap();
        assert_eq!(v.len(), 1);
        // wrapper
        let v: Vec<FileReferenceCandidate> = parse_array(
            serde_json::json!({"items": [{"path": "b.rs", "kind": "file"}]}),
            "t",
        )
        .unwrap();
        assert_eq!(v.len(), 1);
        // bad row skipped, not fatal
        let v: Vec<FileReferenceCandidate> =
            parse_array(serde_json::json!([{"path": "c.rs"}, {"kind": 5}]), "t").unwrap();
        assert_eq!(v.len(), 1);
        // empty tolerated
        let v: Vec<FileReferenceCandidate> = parse_array(serde_json::json!({}), "t").unwrap();
        assert!(v.is_empty());
    }
}
