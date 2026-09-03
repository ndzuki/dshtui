//! session/* 端点封装（V0.1 子集：list / follow / page / cancel，Notes/03 §3/§4）。
//!
//! - `session/list`：cursor 分页（unary）；
//! - `session/follow`：snapshot + 增量事件（流）；
//! - `session/page`：向上翻页前插（unary）；
//! - `session/cancel`：退出前停止运行中会话的最小 wrapper（AC-001-08）；
//! - `session/prompt` 属 REQ-002，不在本层实现。

use serde_json::Value;

use super::envelope::ClientError;
use super::mux::{Mux, StreamHandle};
use super::types::{PageResult, SessionAddress, SessionSeq};
use super::unary;

/// `session/list` 一页结果（容忍 items/nextCursor 字段缺失）。
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

/// 打开 `session/follow` 流；每帧是 `FollowFrame`（snapshot/event）。
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

/// `session/page`：向后分页（前插历史），throughSeq 为 follow 快照 cursor。
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

/// `session/cancel`：停止运行中会话（退出语义 AC-001-08 的最小 wrapper）。
/// 形状未在 Notes/03 字段级记录，按官方注册表 `{sessionId}` 约定，失败不影响退出流程。
pub async fn cancel(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
) -> Result<Value, ClientError> {
    let args = serde_json::json!({ "sessionId": session_id });
    unary(http, base, "session/cancel", args).await
}
