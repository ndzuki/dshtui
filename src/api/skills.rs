//! skills/* endpoint wrappers (REQ-007 V0.4; official read 0.1.2-rc.1 from
//! `dsh-api-session-controller` skill-catalog service).
//!
//! Wire knowledge (centralized here per Notes/03):
//! - `skills/list({sessionId})` — session-scoped catalog of user-invocable
//!   skills (name/description/whenToUse?/modelInvocable). Skills and slash
//!   commands are TWO independent registries; only the composer `/` menu
//!   merges them.
//! - There is NO `skills/execute` RPC: invocation happens by typing `/name`
//!   (host-side dsh-tool-skill injection) — the dshtui skills panel is
//!   read-only + copy-reference, execution goes through the existing
//!   `commands/execute` slash entry.

use super::envelope::ClientError;
use super::types::SkillListValue;
use super::unary;

/// `skills/list` — user-invocable skills visible to one session.
pub async fn list(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
) -> Result<SkillListValue, ClientError> {
    let value = unary(
        http,
        base,
        "skills/list",
        serde_json::json!({ "sessionId": session_id }),
    )
    .await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("skills/list 响应形状异常: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_list_value_parses_entries_and_flags() {
        let v: SkillListValue = serde_json::from_value(serde_json::json!({
            "skills": [
                {"name": "bash", "description": "执行 shell", "modelInvocable": true},
                {"name": "research", "description": "调研",
                 "whenToUse": "需要查证", "modelInvocable": false}
            ]
        }))
        .unwrap();
        assert_eq!(v.skills.len(), 2);
        assert_eq!(v.skills[0].name, "bash");
        assert!(v.skills[0].model_invocable);
        assert_eq!(v.skills[1].when_to_use.as_deref(), Some("需要查证"));
        assert!(!v.skills[1].model_invocable);
    }

    #[test]
    fn skill_list_value_tolerates_missing_fields() {
        let v: SkillListValue =
            serde_json::from_value(serde_json::json!({"skills": [{"name": "x"}]})).unwrap();
        assert_eq!(v.skills[0].description, "");
        assert_eq!(v.skills[0].when_to_use, None);
        assert!(!v.skills[0].model_invocable);
    }
}
