//! session/* endpoint wrappers (V0.1 subset: list / follow / page / cancel,
//! Notes/03 §3/§4).
//!
//! - `session/list`: cursor pagination (unary);
//! - `session/follow`: snapshot + incremental events (stream);
//! - `session/page`: older history prepend (unary);
//! - `session/cancel`: minimal wrapper to stop the running session before
//!   exit (AC-001-08);
//! - `session/prompt` belongs to REQ-002 and is not implemented in this layer.

use serde_json::Value;

use super::envelope::ClientError;
use super::mux::{Mux, StreamHandle};
use super::types::{PageResult, SessionAddress, SessionSeq};
use super::unary;

/// One `session/list` page (tolerant of missing items/nextCursor fields).
#[derive(Debug, Clone, Default)]
pub struct SessionListPage {
    pub raw_items: Vec<super::types::ListItemRaw>,
    pub next_cursor: Option<String>,
}

pub async fn list(
    http: &reqwest::Client,
    base: &str,
    cursor: Option<&str>,
) -> Result<SessionListPage, ClientError> {
    let args = serde_json::json!({ "cursor": cursor });
    let value = unary(http, base, "session/list", args).await?;
    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| value.as_array().cloned().unwrap_or_default());
    let next_cursor = value
        .get("nextCursor")
        .or_else(|| value.get("cursor"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let raw_items = items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect::<Vec<_>>();
    Ok(SessionListPage {
        raw_items,
        next_cursor,
    })
}

/// Open a `session/follow` stream; each frame is a `FollowFrame`
/// (snapshot/event).
pub async fn open_follow(
    mux: &Mux,
    address: &SessionAddress,
    max_messages: usize,
) -> Result<StreamHandle, ClientError> {
    let args = serde_json::json!({
        "address": address,
        "maxMessages": max_messages,
    });
    mux.open_stream("session/follow", args).await
}

/// `session/page`: paginate backwards (prepend history); throughSeq is the
/// follow snapshot cursor.
pub async fn page(
    http: &reqwest::Client,
    base: &str,
    address: &SessionAddress,
    through_seq: SessionSeq,
    before_seq: Option<SessionSeq>,
    max_messages: usize,
) -> Result<PageResult, ClientError> {
    let mut args = serde_json::json!({
        "address": address,
        "throughSeq": through_seq,
        "maxMessages": max_messages,
    });
    if let Some(b) = before_seq {
        args["beforeSeq"] = serde_json::json!(b);
    }
    let value = unary(http, base, "session/page", args).await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("session/page 响应形状异常: {e}")))
}

/// `session/cancel`: stop the running session (minimal wrapper for the
/// AC-001-08 exit semantics).
/// The shape is not recorded field-by-field in Notes/03; follow the official
/// registry `{sessionId}` convention — a failure never breaks the exit flow.
pub async fn cancel(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
) -> Result<Value, ClientError> {
    let args = serde_json::json!({ "sessionId": session_id });
    unary(http, base, "session/cancel", args).await
}

/// One parsed item from the `session/follow` stream. Parsing of the follow
/// frame shape lives HERE (api layer) — the transport loop in main must not
/// interpret FollowFrame/SessionHistoryRecord wire fields itself.
#[derive(Debug, Clone)]
pub enum FollowItem {
    Snapshot {
        cursor: Option<super::types::SessionLogOffset>,
        records: Vec<super::types::SessionHistoryRecord>,
        has_more: bool,
        projections: Option<Value>,
    },
    Event(super::types::SessionWireEvent),
    Chunks(super::types::ChunkRow),
}

/// Parse one `session/follow` stream value (tolerant: unknown frame shapes are
/// logged and skipped).
pub fn parse_follow_item(value: &Value) -> Option<FollowItem> {
    if let Ok(frame) = serde_json::from_value::<super::types::FollowFrame>(value.clone()) {
        return match frame {
            super::types::FollowFrame::Snapshot {
                cursor,
                records,
                has_more,
                projections,
                ..
            } => Some(FollowItem::Snapshot {
                cursor,
                records,
                has_more: has_more.unwrap_or(false),
                projections,
            }),
            super::types::FollowFrame::Event { event } => Some(FollowItem::Event(event)),
        };
    }
    if let Ok(super::types::SessionHistoryRecord::Chunks { event: row }) =
        serde_json::from_value::<super::types::SessionHistoryRecord>(value.clone())
    {
        return Some(FollowItem::Chunks(row));
    }
    tracing::warn!("session/follow 帧形态无法识别，跳过并计入日志");
    None
}
