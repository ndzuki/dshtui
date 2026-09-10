//! session/* endpoint wrappers (V0.1 subset: list / follow / page / cancel /
//! prompt, Notes/03 §3/§4).
//!
//! - `session/list`: cursor pagination (unary);
//! - `session/follow`: snapshot + incremental events (stream);
//! - `session/page`: older history prepend (unary);
//! - `session/cancel`: typed stop request (REQ-002: accepted receipt);
//! - `session/prompt`: typed send request (REQ-002: queue only in V0.1).

use serde::Deserialize;
use serde_json::Value;

use super::envelope::ClientError;
use super::mux::{Mux, StreamHandle};
use super::types::{
    ModelCatalog, PageResult, PromptRequest, SessionAddress, SessionCreateValue, SessionForkValue,
    SessionId, SessionRenameValue, SessionSeq,
};
use super::unary;

/// `{accepted:true}` receipt shared by `session/prompt` / `session/cancel`
/// (official value shape read 2026-09-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedValue {
    pub accepted: bool,
}

/// Parse the accepted receipt and fail-fast on an `accepted=false` rejection
/// (AC-002-09: a server-side refusal surfaces as a typed error, never as a
/// silently accepted result).
fn accepted_receipt(method: &str, value: Value) -> Result<AcceptedValue, ClientError> {
    let accepted: AcceptedValue = serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("{method} 响应形状异常: {e}")))?;
    if !accepted.accepted {
        return Err(ClientError::Protocol(format!(
            "{method} 返回 accepted=false（服务端拒绝）"
        )));
    }
    Ok(accepted)
}

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
    // wire 校正（0.1.2-rc.1 实读 + live 冒烟）：`session/list` 参数 wire 是
    // `_request`（带下划线），业务字段嵌套在 args._request 内。cursor 为
    // SessionListRequest{cursor?} 可选字段——None 时整键省略（官方 zod
    // `.optional()` 拒绝 null，live `{"_request":{}}` 实测 ok）。
    let mut request = serde_json::Map::new();
    if let Some(c) = cursor {
        request.insert("cursor".into(), serde_json::json!(c));
    }
    let args = serde_json::json!({ "_request": request });
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
    // wire 校正（0.1.2-rc.1 实读）：`session/follow` mux open 帧 args 按
    // descriptor 嵌套 —— wire=`request`，open payload.args 为
    // `{"request":{address,maxMessages}}`（SessionFollowRequest）。
    let args = serde_json::json!({
        "request": {
            "address": address,
            "maxMessages": max_messages,
        }
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
    // wire 校正（0.1.2-rc.1 实读 + live 冒烟）：`session/page` 单 request 形参，
    // args 嵌套 `{"request":{...}}`（SessionPageRequest：address/throughSeq 必填、
    // beforeSeq/maxMessages 可选）。beforeSeq 仅在 Some 时出现。
    let mut args = serde_json::json!({
        "request": {
            "address": address,
            "throughSeq": through_seq,
            "maxMessages": max_messages,
        }
    });
    if let Some(b) = before_seq {
        args["request"]["beforeSeq"] = serde_json::json!(b);
    }
    let value = unary(http, base, "session/page", args).await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("session/page 响应形状异常: {e}")))
}

/// `session/page` raw variant（D-46 export 兜底）：返回每条 record 的原始 JSON
/// `Value`（不解析成 typed），供 JSONL 逐行落盘（api/main 层直接收集 raw
/// records，不污染 TranscriptWindow）。分页语义与 `page` 一致。
#[derive(Debug, Clone, Default)]
pub struct RawPage {
    pub records: Vec<serde_json::Value>,
    pub has_more: bool,
}

pub async fn page_raw(
    http: &reqwest::Client,
    base: &str,
    address: &SessionAddress,
    through_seq: SessionSeq,
    before_seq: Option<SessionSeq>,
    max_messages: usize,
) -> Result<RawPage, ClientError> {
    // wire 校正（0.1.2-rc.1 实读 + live 冒烟）：`session/page` 单 request 形参，
    // args 嵌套 `{"request":{...}}`（SessionPageRequest：address/throughSeq 必填、
    // beforeSeq/maxMessages 可选）。beforeSeq 仅在 Some 时出现。
    let mut args = serde_json::json!({
        "request": {
            "address": address,
            "throughSeq": through_seq,
            "maxMessages": max_messages,
        }
    });
    if let Some(b) = before_seq {
        args["request"]["beforeSeq"] = serde_json::json!(b);
    }
    let value = unary(http, base, "session/page", args).await?;
    let records = value
        .get("records")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let has_more = value
        .get("hasMore")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(RawPage { records, has_more })
}

/// `session/prompt` typed unary (REQ-002 §3): posts the official camelCase
/// args and returns the `{accepted:true}` receipt. Errors keep
/// `code/message/details` via the shared envelope classification.
pub async fn prompt(
    http: &reqwest::Client,
    base: &str,
    request: &PromptRequest,
) -> Result<AcceptedValue, ClientError> {
    // wire 校正（0.1.2-rc.1 实读）：`session/prompt` 单 request 形参（wire=
    // `request`），业务字段必须嵌套在 args.request 内（SessionPromptRequest
    // {requestId,sessionId,mode,content,clientTimeZone?}），不能平铺。
    let req = serde_json::to_value(request)
        .map_err(|e| ClientError::Protocol(format!("session/prompt 参数序列化失败: {e}")))?;
    let args = serde_json::json!({ "request": req });
    let value = unary(http, base, "session/prompt", args).await?;
    accepted_receipt("session/prompt", value)
}

/// `session/cancel` typed unary (REQ-002 §3): returns the `{accepted:true}`
/// receipt so the interactive stop path can judge the outcome; the quit path
/// stays best-effort.
pub async fn cancel(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
) -> Result<AcceptedValue, ClientError> {
    // wire 校正（0.1.2-rc.1 实读）：`session/cancel` 单 request 形参，业务
    // 字段嵌套在 args.request 内（SessionCancelRequest{sessionId}）。
    let args = serde_json::json!({ "request": { "sessionId": session_id } });
    let value = unary(http, base, "session/cancel", args).await?;
    accepted_receipt("session/cancel", value)
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

/// `session/search` unary (REQ-003 §3): server-side read-only full-history
/// search, does NOT activate the agent. Bounded by an explicit deadline
/// (`unary_with_timeout` — the baseline unary has none).
pub async fn search(
    http: &reqwest::Client,
    base: &str,
    query: &str,
    timeout: std::time::Duration,
) -> Result<super::types::SearchResult, ClientError> {
    // wire 校正（0.1.2-rc.1 实读）：`session/search` 单 request 形参，query
    // 嵌套在 args.request 内（SessionSearchRequest{query}）。
    let args = serde_json::json!({ "request": { "query": query } });
    let value = super::unary_with_timeout(http, base, "session/search", args, timeout).await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("session/search 响应形状异常: {e}")))
}

/// Open a `session/control` stream (REQ-003 §3): live baseline + replacement
/// frames for running/queued state. Control is **Host-wide**（含所有 session 的
/// queues/jobs/projections），descriptor parameters 为空（0.1.2-rc.1 实读
/// dsh-api-session-controller typert.remote-client.js：`session/control`
/// 无 request 参数），因此 mux open 帧 args 为 `{}`——与官方 client
/// `control(signal)` 零参惯例一致。
pub async fn open_control(mux: &super::Mux) -> Result<StreamHandle, ClientError> {
    let args = serde_json::json!({});
    mux.open_stream("session/control", args).await
}

// ---------- REQ-006 session mutation / model endpoints (V0.3) ----------
//
// All single-`request`-parameter endpoints nest their business fields under
// `{"request":{...}}` inside the envelope args (official read 0.1.2-rc.1 from
// dsh-api-session-controller typert.remote-client.js). Protocol knowledge
// stays in this api layer; app/ui never hand-builds these args.

/// `session/modelCatalog` (zero-arg unary, official read 0.1.2-rc.1).
pub async fn model_catalog(
    http: &reqwest::Client,
    base: &str,
) -> Result<ModelCatalog, ClientError> {
    let value = unary(http, base, "session/modelCatalog", serde_json::json!({})).await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("session/modelCatalog 响应形状异常: {e}")))
}

/// `session/selectModel` — select the next model used by the next prompt.
/// Response `{selected:{provider,model,reasoningEffort?}}`.
pub async fn select_model(
    http: &reqwest::Client,
    base: &str,
    session_id: &SessionId,
    provider: &str,
    model: &str,
    reasoning_effort: Option<&str>,
) -> Result<super::types::WireModelSelection, ClientError> {
    let mut req = serde_json::json!({
        "sessionId": session_id.get(),
        "provider": provider,
        "model": model,
    });
    if let Some(effort) = reasoning_effort {
        req["reasoningEffort"] = serde_json::json!(effort);
    }
    let value = unary(
        http,
        base,
        "session/selectModel",
        serde_json::json!({ "request": req }),
    )
    .await?;
    let selected = value
        .get("selected")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    serde_json::from_value(selected)
        .map_err(|e| ClientError::Protocol(format!("session/selectModel 响应形状异常: {e}")))
}

/// `session/fork` — fork a session (optionally at a seq); returns the new
/// session id.
pub async fn fork(
    http: &reqwest::Client,
    base: &str,
    session_id: &SessionId,
    at_seq: Option<u64>,
) -> Result<SessionForkValue, ClientError> {
    let mut req = serde_json::json!({ "sessionId": session_id.get() });
    if let Some(seq) = at_seq {
        req["atSeq"] = serde_json::json!(seq);
    }
    let value = unary(
        http,
        base,
        "session/fork",
        serde_json::json!({ "request": req }),
    )
    .await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("session/fork 响应形状异常: {e}")))
}

/// `session/rename` — rename a session; returns the normalized title and the
/// committing event seq.
pub async fn rename(
    http: &reqwest::Client,
    base: &str,
    session_id: &SessionId,
    title: &str,
) -> Result<SessionRenameValue, ClientError> {
    let value = unary(
        http,
        base,
        "session/rename",
        serde_json::json!({ "request": { "sessionId": session_id.get(), "title": title } }),
    )
    .await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("session/rename 响应形状异常: {e}")))
}

/// `session/create` — create a new session (optionally in a workspace /
/// cwd); returns the new session id.
pub async fn create(
    http: &reqwest::Client,
    base: &str,
    workspace_id: Option<&str>,
    cwd: Option<&str>,
) -> Result<SessionCreateValue, ClientError> {
    let mut req = serde_json::json!({});
    if let Some(ws) = workspace_id {
        req["workspaceId"] = serde_json::json!(ws);
    }
    if let Some(c) = cwd {
        req["cwd"] = serde_json::json!(c);
    }
    let value = unary(
        http,
        base,
        "session/create",
        serde_json::json!({ "request": req }),
    )
    .await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("session/create 响应形状异常: {e}")))
}

/// Parse one `session/control` stream value. The baseline carries
/// queues/jobs/projections; replacement frames arrive per key. Unknown shapes
/// are preserved as `Unknown` (never silently dropped).
pub fn parse_control_item(value: &Value) -> Option<super::types::ControlItem> {
    use super::types::ControlItem;
    let get = |key: &str| value.get(key).cloned().unwrap_or(Value::Null);
    if let Some(q) = value.get("queues") {
        return Some(ControlItem::Baseline {
            queues: q.clone(),
            jobs: get("jobs"),
            projections: get("projections"),
            raw: value.clone(),
        });
    }
    if let Some(q) = value.get("queue") {
        return Some(ControlItem::Queue { queue: q.clone() });
    }
    if let Some(j) = value.get("jobs") {
        return Some(ControlItem::Jobs { jobs: j.clone() });
    }
    if let Some(p) = value.get("projection") {
        return Some(ControlItem::Projection {
            projection: p.clone(),
        });
    }
    value
        .get("type")
        .and_then(|t| t.as_str())
        .map(|kind| ControlItem::Unknown {
            kind: kind.to_string(),
            raw: value.clone(),
        })
}

/// Parse a `jobs` payload (from a control baseline `jobs` field or a `jobs`
/// replacement frame) into a flat job list.
///
/// Tolerant shapes (0.1.2-rc.1 control frames `[未验证]`, contract smoke
/// locks them):
/// - a bare array `[SessionJob, ...]` (replacement frame);
/// - an object `{ "sess-1": [SessionJob, ...], ... }` (per-session baseline);
/// - `{items: [...]}` wrapper.
///
/// Unknown/undecodable rows are skipped with a warning (never fatal).
pub fn parse_jobs(value: &Value) -> Vec<super::types::SessionJob> {
    let mut out = Vec::new();
    let mut collect = |arr: &[Value]| {
        for v in arr {
            match serde_json::from_value::<super::types::SessionJob>(v.clone()) {
                Ok(job) => out.push(job),
                Err(e) => tracing::warn!(error = %e, "jobs 帧一条解析失败，跳过"),
            }
        }
    };
    match value {
        Value::Array(arr) => collect(arr),
        Value::Object(map) => {
            if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
                collect(items);
            } else {
                // per-session baseline: {"sessionId": [jobs]}
                for (_k, v) in map {
                    if let Some(arr) = v.as_array() {
                        collect(arr);
                    }
                }
            }
        }
        _ => {}
    }
    out
}
