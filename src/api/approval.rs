//! approval/request waterfall events + outcome replies (REQ-003 §3; D-18
//! event-source correction).
//!
//! Protocol knowledge (centralized per Notes/03): the host forwards
//! agent-scope `approval/request` events to the client over `/api/remote.mux`
//! as server-pushed frames (no streamId → the mux push bypass). The client
//! answers through the remote-event outcome channel carrying
//! `{clientId, eventId, outcome:{value}}`; `ApprovalOutcome` is the official
//! four-value vocabulary. The exact endpoint/frame field names are `[未验证]`
//! against the target dsh web version — they are isolated in the constants
//! below so a contract-smoke correction touches only this file (AC-003-18
//! fallback covers unprogrammable targets).

use serde_json::Value;

use super::envelope::ClientError;
use super::types::{ApprovalEvent, ApprovalOutcome};

/// Provisional outcome-reply method (unary envelope). Contract smoke against
/// the target dsh web version locks or replaces this constant.
pub const OUTCOME_METHOD: &str = "remote-event/outcome";

/// Default deadline for search / outcome-reply unaries (REQ-003 §6: must not
/// hang the frame loop).
pub const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Parse a forwarded `approval/request` event from a raw pushed frame.
///
/// Candidate-key tolerant: the identity keys may live at the top level or
/// nested under `event`; only the `type` discriminator is required. Returns
/// `None` for any non-approval frame (the push channel may carry other
/// server-pushed kinds).
pub fn parse_event(raw: &Value) -> Option<ApprovalEvent> {
    let kind = raw.get("type").and_then(|v| v.as_str())?;
    if !kind.contains("approval/request") {
        return None;
    }
    let lookup = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| raw.get(k))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let client_id = lookup(&["clientId", "client_id"]).or_else(|| {
        raw.get("event")
            .and_then(|e| lookup_in(e, &["clientId", "client_id"]))
    })?;
    let event_id = lookup(&["eventId", "event_id"]).or_else(|| {
        raw.get("event")
            .and_then(|e| lookup_in(e, &["eventId", "event_id"]))
    })?;
    Some(ApprovalEvent {
        client_id,
        event_id,
        raw: raw.clone(),
    })
}

fn lookup_in(event: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| event.get(k))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Reply with a decision through the remote-event outcome channel (unary,
/// explicit deadline). Failures surface as ClientError; the caller decides
/// fail-closed behaviour (AC-003-17).
pub async fn reply(
    http: &reqwest::Client,
    base: &str,
    event: &ApprovalEvent,
    outcome: ApprovalOutcome,
    timeout: std::time::Duration,
) -> Result<(), ClientError> {
    let args = serde_json::json!({
        "clientId": event.client_id,
        "eventId": event.event_id,
        "outcome": { "value": serde_json::to_value(outcome).unwrap_or(Value::Null) }
    });
    super::unary_with_timeout(http, base, OUTCOME_METHOD, args, timeout).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_accepts_top_level_identity_keys() {
        let raw = serde_json::json!({
            "type": "approval/request",
            "clientId": "c-1",
            "eventId": "e-1",
            "agent": {"kind": "tool", "name": "bash"}
        });
        let ev = parse_event(&raw).expect("approval frame parsed");
        assert_eq!(ev.client_id, "c-1");
        assert_eq!(ev.event_id, "e-1");
        assert_eq!(ev.raw["agent"]["name"], "bash");
    }

    #[test]
    fn parse_event_accepts_nested_identity_keys() {
        let raw = serde_json::json!({
            "type": "approval/request",
            "event": {"clientId": "c-2", "eventId": "e-2", "signal": "waterfall"}
        });
        let ev = parse_event(&raw).expect("nested identity parsed");
        assert_eq!(ev.client_id, "c-2");
        assert_eq!(ev.event_id, "e-2");
    }

    #[test]
    fn parse_event_rejects_non_approval_frames() {
        assert!(parse_event(&serde_json::json!({"type": "toast", "message": "hi"})).is_none());
        assert!(parse_event(&serde_json::json!({"type": "item", "streamId": 1})).is_none());
        // Missing identity keys → None (never a half-built event).
        assert!(parse_event(&serde_json::json!({"type": "approval/request"})).is_none());
    }

    #[test]
    fn outcome_wire_values_are_kebab_case() {
        assert_eq!(
            serde_json::to_value(ApprovalOutcome::AllowedOnce).unwrap(),
            "allowed-once"
        );
        assert_eq!(
            serde_json::to_value(ApprovalOutcome::Rejected).unwrap(),
            "rejected"
        );
        assert_eq!(
            serde_json::to_value(ApprovalOutcome::Cancelled).unwrap(),
            "cancelled"
        );
        assert_eq!(
            serde_json::to_value(ApprovalOutcome::Unavailable).unwrap(),
            "unavailable"
        );
        assert_eq!(ApprovalOutcome::AllowedOnce.as_str(), "allowed-once");
    }
}
