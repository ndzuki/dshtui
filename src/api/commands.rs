//! commands/* endpoint wrappers (REQ-006 §3 slash commands; V0.3).
//!
//! Protocol knowledge (centralized here per Notes/03; official read
//! 0.1.2-rc.1 from dsh-commands typert.remote-client.js):
//! - both endpoints are scoped to an **agent** via `agentId` (a flat args key,
//!   NOT nested under `request`);
//! - `commands/list` has no query/prefix — the command table is dynamically
//!   registered, so Tab completion is a local prefix filter over the fetched
//!   descriptors (AC-006-04);
//! - `commands/execute` may resolve to `undefined` (the server decided there
//!   is nothing to report); the api layer tolerates that as an empty success.

use super::envelope::ClientError;
use super::types::{CommandDescriptor, CommandExecution};
use super::unary;

/// `commands/list` — the slash-command descriptors for one agent.
pub async fn list(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
) -> Result<Vec<CommandDescriptor>, ClientError> {
    let value = unary(
        http,
        base,
        "commands/list",
        serde_json::json!({ "agentId": agent_id }),
    )
    .await?;
    match value {
        serde_json::Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|v| serde_json::from_value::<CommandDescriptor>(v).ok())
            .collect()),
        other => Err(ClientError::Protocol(format!(
            "commands/list 响应形状异常（期望数组，得到 {}）",
            serde_json::to_string(&other).unwrap_or_default()
        ))),
    }
}

/// `commands/execute` — run one slash command line for an agent. `images` is
/// required by the wire but empty in V0.3 (no image slash commands yet).
/// A missing/`undefined` execution value maps to `Ok(None)` (tolerant).
pub async fn execute(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    line: &str,
    images: &[serde_json::Value],
) -> Result<Option<CommandExecution>, ClientError> {
    let args = serde_json::json!({
        "agentId": agent_id,
        "line": line,
        "images": images,
    });
    let value = unary(http, base, "commands/execute", args).await?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| ClientError::Protocol(format!("commands/execute 响应形状异常: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_descriptor_parses_optional_input() {
        let v: CommandDescriptor = serde_json::from_value(serde_json::json!({
            "name": "plan",
            "description": "Plan mode on/off",
            "input": {"hint": "off", "images": false}
        }))
        .unwrap();
        assert_eq!(v.name, "plan");
        assert_eq!(v.input.as_ref().unwrap().hint, "off");
        assert_eq!(v.input.as_ref().unwrap().images, Some(false));
    }

    #[test]
    fn command_descriptor_tolerates_missing_input() {
        let v: CommandDescriptor =
            serde_json::from_value(serde_json::json!({"name": "help", "description": "h"}))
                .unwrap();
        assert_eq!(v.name, "help");
        assert!(v.input.is_none());
    }

    #[test]
    fn command_execution_parses_success_and_error_kinds() {
        let ok: CommandExecution = serde_json::from_value(serde_json::json!({
            "commandId": "c1",
            "result": {"kind": "success", "text": "ok", "sourceEventSeq": 7}
        }))
        .unwrap();
        assert_eq!(ok.command_id, "c1");
        assert_eq!(ok.result.as_ref().unwrap().kind, "success");
        assert_eq!(ok.result.as_ref().unwrap().source_event_seq, Some(7));

        let err: CommandExecution = serde_json::from_value(serde_json::json!({
            "commandId": "c2",
            "result": {"kind": "error", "text": "boom"}
        }))
        .unwrap();
        assert_eq!(err.result.as_ref().unwrap().kind, "error");
    }
}
