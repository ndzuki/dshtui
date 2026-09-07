//! goals/* endpoint wrappers (REQ-007 V0.4; official read 0.1.2-rc.1 from
//! `@deepseek-ai/dsh-goal/typert.remote-client`).
//!
//! Wire knowledge (centralized here per Notes/03):
//! - every verb takes `agentId` as its first flat arg (`scope.context=agent`,
//!   wire `agentId` — same convention as commands/list);
//! - there is NO list/query endpoint: a goal is a per-session singleton read
//!   from the `goal` projection; mutations carry `ref: GoalRef{id, revision}`
//!   (CAS — revision increments on every mutation);
//! - `create` nests its business fields under `{"request":{...}}`
//!   (single-`request`-parameter convention); pause/resume/complete/clear
//!   take `agentId` + `ref` flat.

use serde_json::Value;

use super::envelope::ClientError;
use super::types::{CreateGoalRequest, GoalRef, GoalSnapshot};
use super::unary;

/// `goals/create` — create the session singleton goal.
pub async fn create(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    request: &CreateGoalRequest,
) -> Result<GoalRef, ClientError> {
    let req = serde_json::to_value(request)
        .map_err(|e| ClientError::Protocol(format!("goals/create 参数序列化失败: {e}")))?;
    let value = unary(
        http,
        base,
        "goals/create",
        serde_json::json!({ "agentId": agent_id, "request": req }),
    )
    .await?;
    parse_ref("goals/create", value)
}

/// `goals/pause` — pause the singleton goal.
pub async fn pause(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    ref_: &GoalRef,
) -> Result<GoalSnapshot, ClientError> {
    let value = unary(
        http,
        base,
        "goals/pause",
        serde_json::json!({ "agentId": agent_id, "ref": ref_ }),
    )
    .await?;
    parse_snapshot("goals/pause", value)
}

/// `goals/resume` — resume a paused goal.
pub async fn resume(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    ref_: &GoalRef,
) -> Result<GoalSnapshot, ClientError> {
    let value = unary(
        http,
        base,
        "goals/resume",
        serde_json::json!({ "agentId": agent_id, "ref": ref_ }),
    )
    .await?;
    parse_snapshot("goals/resume", value)
}

/// `goals/complete` — complete the goal.
pub async fn complete(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    ref_: &GoalRef,
) -> Result<GoalSnapshot, ClientError> {
    let value = unary(
        http,
        base,
        "goals/complete",
        serde_json::json!({ "agentId": agent_id, "ref": ref_ }),
    )
    .await?;
    parse_snapshot("goals/complete", value)
}

/// `goals/clear` — clear the goal (returns the cleared ref).
pub async fn clear(
    http: &reqwest::Client,
    base: &str,
    agent_id: &str,
    ref_: &GoalRef,
) -> Result<GoalRef, ClientError> {
    let value = unary(
        http,
        base,
        "goals/clear",
        serde_json::json!({ "agentId": agent_id, "ref": ref_ }),
    )
    .await?;
    parse_ref("goals/clear", value)
}

/// Tolerant goal-ref parser: the response may be a bare `{ref:{...}}` wrapper
/// (0.1.2-rc.1 `CreateGoalResult`) or a plain GoalRef.
fn parse_ref(method: &str, value: Value) -> Result<GoalRef, ClientError> {
    let inner = value.get("ref").cloned().unwrap_or(value);
    serde_json::from_value(inner)
        .map_err(|e| ClientError::Protocol(format!("{method} 响应形状异常: {e}")))
}

/// Tolerant goal-view parser: prefer `{goal:{...}}` projection shape; accept a
/// bare GoalSnapshot as fallback (shape `[未验证]`; contract smoke locks it).
fn parse_snapshot(method: &str, value: Value) -> Result<GoalSnapshot, ClientError> {
    let inner = value.get("goal").cloned().unwrap_or_else(|| value.clone());
    serde_json::from_value(inner)
        .map_err(|e| ClientError::Protocol(format!("{method} 响应形状异常: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::GoalPhase;

    #[test]
    fn goal_ref_and_phase_round_trip() {
        let r: GoalRef =
            serde_json::from_value(serde_json::json!({"id": "g1", "revision": 3})).unwrap();
        assert_eq!(r.id, "g1");
        assert_eq!(r.revision, 3);
        let p: GoalPhase = serde_json::from_str("\"paused\"").unwrap();
        assert_eq!(p, GoalPhase::Paused);
        let s: GoalSnapshot = serde_json::from_value(serde_json::json!({
            "id": "g1", "revision": 4, "objective": "交付 REQ-007",
            "phase": "active", "maxGoalRounds": 5
        }))
        .unwrap();
        assert_eq!(s.phase, Some(GoalPhase::Active));
        assert_eq!(s.max_goal_rounds, Some(5));
    }

    #[test]
    fn create_goal_request_serializes_objective_and_optional_rounds() {
        let req = CreateGoalRequest {
            objective: "交付".into(),
            max_goal_rounds: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["objective"], "交付");
        assert!(v.get("maxGoalRounds").is_none(), "None 不上送");
    }

    #[test]
    fn parse_ref_tolerates_wrapped_and_bare_shapes() {
        let wrapped: GoalRef = parse_ref(
            "goals/create",
            serde_json::json!({"ref": {"id": "g1", "revision": 1}}),
        )
        .unwrap();
        assert_eq!(wrapped.id, "g1");
        let bare: GoalRef = parse_ref("goals/clear", serde_json::json!({"id": "g1"})).unwrap();
        assert_eq!(bare.revision, 0, "缺 revision 容忍为 0");
    }
}
