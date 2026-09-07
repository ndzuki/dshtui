//! messageFeedback/* endpoint wrappers (REQ-007 V0.4; official read
//! 0.1.2-rc.1).
//!
//! Wire knowledge (centralized here per Notes/03):
//! - `messageFeedback/put` — CAS write of one rating on an assistant message
//!   (`ifVersion` guards against racing edits; messageId = assistant
//!   `message.id`);
//! - `messageFeedback/delete` — clear a rating;
//! - `messageFeedback/list` — read back ratings.
//!
//! Retry semantics: rating a message is an idempotent client-minted action —
//! the reducer carries requestId single-flight; permission errors are never
//! auto-retried (Notes/03 §8).

use serde_json::Value;

use super::envelope::ClientError;
use super::types::{MessageFeedbackItem, MessageFeedbackPutRequest};
use super::unary;

/// `messageFeedback/put` — set/update a rating (CAS via ifVersion).
pub async fn put(
    http: &reqwest::Client,
    base: &str,
    request: &MessageFeedbackPutRequest,
) -> Result<Value, ClientError> {
    let req = serde_json::to_value(request)
        .map_err(|e| ClientError::Protocol(format!("messageFeedback/put 参数序列化失败: {e}")))?;
    let value = unary(
        http,
        base,
        "messageFeedback/put",
        serde_json::json!({ "request": req }),
    )
    .await?;
    Ok(value)
}

/// `messageFeedback/delete` — remove the rating on one message.
pub async fn delete(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
    message_id: &str,
) -> Result<Value, ClientError> {
    let value = unary(
        http,
        base,
        "messageFeedback/delete",
        serde_json::json!({ "request": {
            "sessionId": session_id,
            "messageId": message_id,
        }}),
    )
    .await?;
    Ok(value)
}

/// `messageFeedback/list` — ratings for one session.
pub async fn list(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
) -> Result<Vec<MessageFeedbackItem>, ClientError> {
    let value = unary(
        http,
        base,
        "messageFeedback/list",
        serde_json::json!({ "sessionId": session_id }),
    )
    .await?;
    let arr = value
        .as_array()
        .cloned()
        .or_else(|| value.get("items").and_then(|v| v.as_array()).cloned())
        .or_else(|| value.get("feedback").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        match serde_json::from_value::<MessageFeedbackItem>(v) {
            Ok(item) => out.push(item),
            Err(e) => tracing::warn!(error = %e, "messageFeedback/list 一条解析失败，跳过"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_request_serializes_cas_fields() {
        let req = MessageFeedbackPutRequest {
            session_id: "s1".into(),
            message_id: "m1".into(),
            rating: "positive".into(),
            note: Some("很好".into()),
            if_version: Some(2),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["sessionId"], "s1");
        assert_eq!(v["messageId"], "m1");
        assert_eq!(v["rating"], "positive");
        assert_eq!(v["note"], "很好");
        assert_eq!(v["ifVersion"], 2);
    }

    #[test]
    fn feedback_item_parses_and_tolerates_unknown() {
        let v: MessageFeedbackItem =
            serde_json::from_value(serde_json::json!({"messageId": "m1", "rating": "negative"}))
                .unwrap();
        assert_eq!(v.message_id, "m1");
        assert_eq!(v.rating, "negative");
        assert_eq!(v.version, None);
    }

    #[test]
    fn list_tolerates_wrappers_and_bad_rows() {
        // bare array
        let v = list_parse(serde_json::json!([{"messageId": "m1", "rating": "positive"}])).unwrap();
        assert_eq!(v.len(), 1);
        // wrapper + bad row skipped
        let v = list_parse(serde_json::json!({
            "items": [{"messageId": "m1"}, {"rating": 3}]
        }))
        .unwrap();
        assert_eq!(v.len(), 1);
        // empty
        let v = list_parse(serde_json::json!({})).unwrap();
        assert!(v.is_empty());
    }

    fn list_parse(v: Value) -> Result<Vec<MessageFeedbackItem>, ClientError> {
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v.get("items").and_then(|x| x.as_array()).cloned())
            .or_else(|| v.get("feedback").and_then(|x| x.as_array()).cloned())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(arr.len());
        for x in arr {
            if let Ok(item) = serde_json::from_value::<MessageFeedbackItem>(x) {
                out.push(item);
            }
        }
        Ok(out)
    }
}
