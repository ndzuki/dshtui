//! `session/attachment` endpoint wrapper (Notes/03 §6; REQ-004 §3/§5).
//!
//! The wire schema is decoded inside this module and converted to typed
//! attachment data before crossing the API boundary. Callers never inspect
//! wire JSON structs directly.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use super::envelope::ClientError;
use super::types::{AttachmentId, MediaType, SessionId};
use super::unary;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentWire {
    pub attachment_id: String,
    #[serde(default)]
    pub media_type: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub width: u64,
    #[serde(default)]
    pub height: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub original_dimensions: Option<OriginalDimensions>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OriginalDimensions {
    #[serde(default)]
    pub width: u64,
    #[serde(default)]
    pub height: u64,
}

/// Fetched attachment metadata and decoded image bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentData {
    pub attachment_id: AttachmentId,
    pub media_type: MediaType,
    /// Server-reported encoded size. Cache accounting uses `image_bytes.len()`.
    pub bytes: u64,
    pub width: u64,
    pub height: u64,
    pub name: Option<String>,
    pub original_dimensions: Option<OriginalDimensions>,
    pub image_bytes: Vec<u8>,
}

/// Parse the unary `value` of a `session/attachment` ok response.
/// Missing attachment / undecodable base64 are protocol-shape errors.
pub fn parse_response(value: &Value) -> Result<AttachmentData, ClientError> {
    let wire: AttachmentWire = value
        .get("attachment")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|e| {
            ClientError::Protocol(format!("session/attachment attachment 对象解析失败: {e}"))
        })?
        .ok_or_else(|| ClientError::Protocol("session/attachment 响应缺少 attachment".into()))?;
    let data = value
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ClientError::Protocol("session/attachment 响应缺少 data".into()))?;
    let image_bytes = BASE64_STANDARD.decode(data.as_bytes()).map_err(|e| {
        ClientError::Protocol(format!("session/attachment data base64 解码失败: {e}"))
    })?;
    Ok(AttachmentData {
        attachment_id: AttachmentId::new(wire.attachment_id),
        media_type: MediaType::new(wire.media_type),
        bytes: wire.bytes,
        width: wire.width,
        height: wire.height,
        name: wire.name,
        original_dimensions: wire.original_dimensions,
        image_bytes,
    })
}

/// `session/attachment` unary call. `attachmentId` is opaque and is never
/// interpreted as a path or URL (REQ-004 §7).
pub async fn fetch(
    http: &reqwest::Client,
    base: &str,
    session_id: &SessionId,
    attachment_id: &AttachmentId,
) -> Result<AttachmentData, ClientError> {
    // wire 校正（0.1.2-rc.1 实读）：`session/attachment` 单 request 形参，
    // sessionId/attachmentId 嵌套在 args.request 内
    // （SessionAttachmentRequest{sessionId,attachmentId}）。
    let args = serde_json::json!({
        "request": {
            "sessionId": session_id.get(),
            "attachmentId": attachment_id.get(),
        }
    });
    let value = unary(http, base, "session/attachment", args).await?;
    parse_response(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_response_decodes_attachment_and_data() {
        let value = json!({
            "attachment": {
                "attachmentId": "att-1",
                "mediaType": "image/png",
                "bytes": 4,
                "width": 10,
                "height": 20,
                "name": "a.png",
                "originalDimensions": {"width": 100, "height": 200}
            },
            "data": "AQIDBA=="
        });
        let parsed = parse_response(&value).unwrap();
        assert_eq!(parsed.attachment_id.get(), "att-1");
        assert_eq!(parsed.media_type.get(), "image/png");
        assert_eq!(parsed.bytes, 4);
        assert_eq!(parsed.width, 10);
        assert_eq!(parsed.name.as_deref(), Some("a.png"));
        assert_eq!(parsed.image_bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn parse_response_tolerates_unknown_and_missing_optional_fields() {
        let value = json!({
            "attachment": {"attachmentId": "att-2", "mediaType": "image/gif"},
            "data": "",
            "futureField": {"anything": true}
        });
        let parsed = parse_response(&value).unwrap();
        assert_eq!(parsed.attachment_id.get(), "att-2");
        assert!(parsed.name.is_none());
        assert!(parsed.original_dimensions.is_none());
        assert!(parsed.image_bytes.is_empty());
    }

    #[test]
    fn parse_response_missing_attachment_is_protocol_error() {
        let err = parse_response(&json!({"data": "AA=="})).unwrap_err();
        assert!(matches!(err, ClientError::Protocol(ref m) if m.contains("缺少 attachment")));
        assert_eq!(err.class(), super::super::envelope::ErrorClass::UserFacing);
    }

    #[test]
    fn parse_response_invalid_base64_is_protocol_error() {
        let value = json!({
            "attachment": {"attachmentId": "att-3", "mediaType": "image/png"},
            "data": "!!!not-base64!!!"
        });
        let err = parse_response(&value).unwrap_err();
        assert!(matches!(err, ClientError::Protocol(ref m) if m.contains("base64 解码失败")));
    }

    #[test]
    fn parse_response_malformed_attachment_object_is_protocol_error() {
        let err = parse_response(&json!({"attachment": {"attachmentId": 42}, "data": "AA=="}))
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Protocol(ref m) if m.contains("attachment 对象解析失败"))
        );
    }
}
