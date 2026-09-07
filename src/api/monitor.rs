//! OTR agent-server 明文 REST 客户端（REQ-009 §2/§3，D-29：直连
//! `127.0.0.1:8799`，**非**官方 dsh Remote API envelope 域）。
//!
//! - `GET /health` / `GET /agents`（+ `x-agents-finished` 头）/ `GET /kb-stats`
//!   （嵌套 hist）/ `POST /agent/chat`；
//! - wire → 内部解析用 `#[serde(default)]` 容忍未知字段（`03 §7` 兼容口径）；
//! - 错误 thiserror 分类（网络 / 非 200 / JSON / 业务 errorCode），指数退避
//!   复用 `api/envelope.rs` `Backoff`；
//! - tracing 脱敏：chat 内容 / kbQuery / 项目名不进日志明文（REQ-009 §6）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::envelope::ErrorClass;

/// agent-server 默认地址（config 注入，此处仅兜底）。
pub const DEFAULT_AGENT_SERVER_ADDR: &str = "http://127.0.0.1:8799";

/// 直连 8799 的 reqwest 客户端（无 cookie/auth；rustls-tls、30s 超时、
/// 禁 redirect —— 与 `api/mod.rs:36-44` 构建模式一致）。
#[derive(Clone)]
pub struct MonitorClient {
    pub http: reqwest::Client,
    pub base: String,
}

impl MonitorClient {
    pub fn new(addr: &str) -> Result<Self, MonitorError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| MonitorError::Transport(format!("HTTP 客户端构建失败: {e}")))?;
        Ok(Self {
            http,
            base: addr.trim_end_matches('/').to_string(),
        })
    }

    /// `GET /health` → `{ok:true}`（启动探测，AC-009-01）。
    pub async fn health(&self) -> Result<(), MonitorError> {
        let url = format!("{}/health", self.base);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| MonitorError::Transport(format!("health 请求失败（{url}）: {e}")))?;
        if !resp.status().is_success() {
            return Err(MonitorError::http(resp.status().as_u16(), "health"));
        }
        let body: HealthWire = resp
            .json()
            .await
            .map_err(|e| MonitorError::Json(format!("health 响应解析失败: {e}")))?;
        if !body.ok {
            return Err(MonitorError::Business {
                error_code: "health-not-ok".into(),
                message: "agent-server health 返回 ok=false".into(),
            });
        }
        Ok(())
    }

    /// `GET /agents` → 数组 + `x-agents-finished` 响应头（FR-009-02）。
    pub async fn agents(&self) -> Result<AgentsResponse, MonitorError> {
        let url = format!("{}/agents", self.base);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| MonitorError::Transport(format!("/agents 请求失败（{url}）: {e}")))?;
        if !resp.status().is_success() {
            return Err(MonitorError::http(resp.status().as_u16(), "/agents"));
        }
        let finished = resp
            .headers()
            .get("x-agents-finished")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let entries: Vec<WireAgent> = resp
            .json()
            .await
            .map_err(|e| MonitorError::Json(format!("/agents 响应解析失败: {e}")))?;
        Ok(AgentsResponse { entries, finished })
    }

    /// `GET /kb-stats` → 嵌套 hist 口径（D-29 fact 修正：`totals.hist` /
    /// `window.hist` 各自携带 `{boundaries,counts}`）。
    pub async fn kb_stats(&self) -> Result<WireKbStats, MonitorError> {
        let url = format!("{}/kb-stats", self.base);
        let resp =
            self.http.get(&url).send().await.map_err(|e| {
                MonitorError::Transport(format!("/kb-stats 请求失败（{url}）: {e}"))
            })?;
        if !resp.status().is_success() {
            return Err(MonitorError::http(resp.status().as_u16(), "/kb-stats"));
        }
        resp.json()
            .await
            .map_err(|e| MonitorError::Json(format!("/kb-stats 响应解析失败: {e}")))
    }

    /// `POST /agent/chat` 一问一答（同 agent 多轮复用 sessionId，AC-009-06）。
    /// chat 内容不进日志明文（REQ-009 §6 脱敏口径）。
    pub async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, MonitorError> {
        let url = format!("{}/agent/chat", self.base);
        tracing::debug!(session_id = %req.session_id.as_deref().unwrap_or("<new>"), "agent/chat 发送（内容脱敏）");
        let resp =
            self.http.post(&url).json(req).send().await.map_err(|e| {
                MonitorError::Transport(format!("agent/chat 请求失败（{url}）: {e}"))
            })?;
        if !resp.status().is_success() {
            // 非 200：server 以 `{error}` 文本返回（agent-server.mjs catch 分支），
            // 读取首段作为可读错误提示。
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let brief: String = text.chars().take(200).collect();
            return Err(MonitorError::Http {
                status,
                endpoint: "agent/chat".into(),
                detail: brief,
            });
        }
        resp.json()
            .await
            .map_err(|e| MonitorError::Json(format!("agent/chat 响应解析失败: {e}")))
    }
}

/// `/agents` 返回：条目数组 + 最近完成计数。
#[derive(Debug, Clone, Default)]
pub struct AgentsResponse {
    pub entries: Vec<WireAgent>,
    pub finished: u32,
}

/// `/agents` 条目 wire 形状（`agent-server.mjs listAgents()` L1623-1652 实读
/// 16 字段；未知字段容忍）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct WireAgent {
    pub session_id: String,
    pub phase: String,
    pub task: String,
    pub project: String,
    pub task_id: String,
    /// "idle" | "working"。
    pub status: String,
    pub task_status: String,
    pub elapsed: i64,
    pub last_event_at: i64,
    pub seq: i64,
    pub label: String,
    pub kind: String,
    pub parent_session_id: String,
    pub delegation_depth: i64,
    pub provider: String,
    pub model: String,
}

/// `GET /health` wire。
#[derive(Debug, Clone, Deserialize)]
pub struct HealthWire {
    pub ok: bool,
}

/// `/kb-stats` wire：累计 + 当前小时窗口，各带嵌套 hist（D-29 口径）。
/// ⚠️ `restored` 实读为 **restoredAt 时间戳**（`kbStatsPersist.restoredAt`，
/// 2026-09-07 实机冒烟 `"restored":1788432607839.9048`），非 bool——truthy
/// 反序列化（数字 >0 或 true 视为已恢复），契约容忍（03 §7）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct WireKbStats {
    pub totals: WireKbBucket,
    pub window: WireKbBucket,
    pub last_log_at: i64,
    #[serde(deserialize_with = "deser_truthy")]
    pub restored: bool,
}

/// truthy：`true` 或 >0 数字 → true；false/0/null → false（字段漂移容忍）。
fn deser_truthy<'de, D>(de: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(de)?;
    Ok(match v {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f > 0.0),
        _ => false,
    })
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct WireKbBucket {
    pub hits: i64,
    pub misses: i64,
    /// `empty` 为 Rust 关键字，serde rename 保持 wire 名不变。
    #[serde(rename = "empty")]
    pub empty: i64,
    pub errs: i64,
    pub skipped: i64,
    pub searches: i64,
    pub avg_ms: i64,
    pub hist: WireHistogram,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct WireHistogram {
    /// 桶边界 = agent-server 常量 `KB_DURATION_BOUNDARIES`（AC-009-07）。
    pub boundaries: Vec<i64>,
    pub counts: Vec<i64>,
}

/// `POST /agent/chat` 请求体（D-29：KB-first + 项目上下文由服务端注入，
/// 客户端只传参）。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    pub message: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kb_query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `POST /agent/chat` 响应：`{text, outcome, sessionId, errorCode?, error?}`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatResponse {
    pub text: String,
    pub outcome: String,
    pub session_id: String,
    pub error_code: String,
    pub error: String,
}

// ---------- 错误分类（REQ-009 §6） ----------

#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("网络传输失败: {0}")]
    Transport(String),
    #[error("HTTP {status}（{endpoint}）{detail}")]
    Http {
        status: u16,
        endpoint: String,
        detail: String,
    },
    #[error("响应解析失败: {0}")]
    Json(String),
    #[error("业务错误 [{error_code}]: {message}")]
    Business { error_code: String, message: String },
}

impl MonitorError {
    fn http(status: u16, endpoint: &str) -> Self {
        MonitorError::Http {
            status,
            endpoint: endpoint.to_string(),
            detail: String::new(),
        }
    }

    /// 是否可自动重试（轮询退避用；业务错误与 4xx 不自动重试）。
    pub fn is_retryable(&self) -> bool {
        match self {
            MonitorError::Transport(_) => true,
            MonitorError::Http { status, .. } => *status >= 500,
            MonitorError::Json(_) => false,
            MonitorError::Business { .. } => false,
        }
    }

    /// 稳定错误码（测试/日志检索口径，对齐 `api::ClientError::code()`）。
    pub fn code(&self) -> String {
        match self {
            MonitorError::Transport(_) => "transport".into(),
            MonitorError::Http { .. } => "http".into(),
            MonitorError::Json(_) => "json".into(),
            MonitorError::Business { error_code, .. } => error_code.clone(),
        }
    }

    /// 分类口径（失败信息可断言：网络失败 retryable、业务错误码 user-facing）。
    pub fn class(&self) -> ErrorClass {
        if self.is_retryable() {
            ErrorClass::Retryable
        } else {
            ErrorClass::UserFacing
        }
    }
}

// ---------- 纯解析函数（测试 seam；容忍未知/缺失字段） ----------

/// 解析 `/agents` JSON 数组 + finished 计数（`03 §7` 兼容口径：数组非数组
/// 时按空处理、单条条目未知字段忽略）。
pub fn parse_agents(body: &str, finished: u32) -> Result<AgentsResponse, MonitorError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| MonitorError::Json(format!("/agents 响应解析失败: {e}")))?;
    let entries = match value {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| serde_json::from_value::<WireAgent>(v).ok())
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    Ok(AgentsResponse { entries, finished })
}

/// 解析 `x-agents-finished` 响应头（非数字/缺失 → 0，不崩溃）。
pub fn parse_finished_header(v: Option<&str>) -> u32 {
    v.and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0)
}

/// 解析 `/kb-stats` 响应体（嵌套 hist 口径；解析失败保留快照由上层处理）。
pub fn parse_kb_stats(body: &str) -> Result<WireKbStats, MonitorError> {
    serde_json::from_str::<WireKbStats>(body)
        .map_err(|e| MonitorError::Json(format!("/kb-stats 响应解析失败: {e}")))
}

/// 解析 `/agent/chat` 响应体。
pub fn parse_chat_response(body: &str) -> Result<ChatResponse, MonitorError> {
    serde_json::from_str::<ChatResponse>(body)
        .map_err(|e| MonitorError::Json(format!("agent/chat 响应解析失败: {e}")))
}

/// `{error}` 文本响应（非 200 时的错误详情，截断 200 字符防刷屏）。
pub fn error_detail(body: &str) -> String {
    body.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agents_full_entry_and_finished() {
        let body = r#"[
          {"sessionId":"session-abc123","phase":"implementing","task":"修管线\n第二行",
           "project":"release-manager","taskId":"TASK-077","status":"working",
           "taskStatus":"implementing","elapsed":3601,"lastEventAt":1788515073345,
           "seq":42,"label":"子代理A","kind":"subagent","parentSessionId":"session-parent",
           "delegationDepth":2,"provider":"deepseek_magic","model":"deepseek-v4-pro",
           "unknownField":"ignored"}
        ]"#;
        let r = parse_agents(body, 7).unwrap();
        assert_eq!(r.finished, 7);
        assert_eq!(r.entries.len(), 1);
        let e = &r.entries[0];
        assert_eq!(e.session_id, "session-abc123");
        assert_eq!(e.task_id, "TASK-077");
        assert_eq!(e.status, "working");
        assert_eq!(e.elapsed, 3601);
        assert_eq!(e.last_event_at, 1788515073345);
        assert_eq!(e.delegation_depth, 2);
        assert_eq!(e.model, "deepseek-v4-pro");
    }

    #[test]
    fn parse_agents_missing_fields_default_to_empty() {
        // 契约形状边界：字段缺失不得 panic（`#[serde(default)]` 容忍）。
        let r = parse_agents(r#"[{"sessionId":"s1"}]"#, 0).unwrap();
        assert_eq!(r.entries[0].session_id, "s1");
        assert_eq!(r.entries[0].task, "");
        assert_eq!(r.entries[0].kind, "");
        assert_eq!(r.entries[0].delegation_depth, 0);
    }

    #[test]
    fn parse_agents_non_array_body_is_empty_not_panic() {
        // 失败场景：非数组（object/null）→ 空条目，不崩溃（AC-009-08）。
        let r = parse_agents(r#"{"oops":true}"#, 3).unwrap();
        assert!(r.entries.is_empty());
        assert_eq!(r.finished, 3);
        let r2 = parse_agents("not json", 0);
        assert!(matches!(r2, Err(MonitorError::Json(_))));
    }

    #[test]
    fn parse_finished_header_tolerates_missing_and_garbage() {
        assert_eq!(parse_finished_header(None), 0);
        assert_eq!(parse_finished_header(Some("12")), 12);
        assert_eq!(parse_finished_header(Some("")), 0);
        assert_eq!(parse_finished_header(Some("abc")), 0);
    }

    #[test]
    fn parse_kb_stats_nested_hist_shape() {
        let body = r#"{"totals":{"hits":10,"misses":2,"empty":1,"errs":0,"skipped":1,
          "searches":12,"avgMs":345,"hist":{"boundaries":[0,100,500,1000,2000,4000,16000],
          "counts":[3,2,1,4,1,1,0]}},
          "window":{"hits":1,"misses":0,"empty":0,"errs":0,"skipped":0,"searches":1,
          "avgMs":120,"hist":{"boundaries":[0,100,500,1000,2000,4000,16000],
          "counts":[0,0,0,0,1,0,0]}},
          "lastLogAt":1788515073345,"restored":true}"#;
        let s = parse_kb_stats(body).unwrap();
        assert_eq!(s.totals.hits, 10);
        assert_eq!(s.totals.empty, 1);
        assert_eq!(s.totals.avg_ms, 345);
        assert_eq!(s.totals.hist.boundaries.len(), 7);
        assert_eq!(s.totals.hist.counts.len(), 7);
        assert_eq!(s.window.hist.counts[4], 1);
        assert!(s.restored);
    }

    #[test]
    fn parse_chat_response_with_and_without_error() {
        let ok =
            parse_chat_response(r#"{"text":"回答","outcome":"completed","sessionId":"session-x"}"#)
                .unwrap();
        assert_eq!(ok.text, "回答");
        assert_eq!(ok.session_id, "session-x");
        assert_eq!(ok.error_code, "");

        let err = parse_chat_response(
            r#"{"text":"","outcome":"error","sessionId":"session-x",
                "errorCode":"TIMEOUT","error":"模型超时"}"#,
        )
        .unwrap();
        assert_eq!(err.outcome, "error");
        assert_eq!(err.error_code, "TIMEOUT");
        assert_eq!(err.error, "模型超时");
    }

    #[test]
    fn chat_request_skips_none_optionals() {
        let req = ChatRequest {
            message: "hi".into(),
            provider: "p".into(),
            model: "m".into(),
            ..ChatRequest::default()
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["message"], "hi");
        assert!(v.get("sessionId").is_none());
        assert!(v.get("kbQuery").is_none());
        assert!(v.get("project").is_none());

        let full = ChatRequest {
            message: "hi".into(),
            provider: "p".into(),
            model: "m".into(),
            reasoning_effort: Some("medium".into()),
            session_id: Some("session-1".into()),
            kb_query: Some("任务标题".into()),
            project: Some("proj".into()),
        };
        let v2 = serde_json::to_value(&full).unwrap();
        assert_eq!(v2["sessionId"], "session-1");
        assert_eq!(v2["kbQuery"], "任务标题");
        assert_eq!(v2["reasoningEffort"], "medium");
    }

    #[test]
    fn parse_kb_stats_restored_truthy_tolerates_bool_and_timestamp() {
        // 实机口径：restored = restoredAt 时间戳（数字）；历史口径：bool。
        let num = parse_kb_stats(
            r#"{"totals":{"hits":0,"misses":0,"empty":0,"errs":0,"skipped":0,
            "searches":0,"avgMs":0,"hist":{"boundaries":[0],"counts":[0]}},
            "window":{"hits":0,"misses":0,"empty":0,"errs":0,"skipped":0,
            "searches":0,"avgMs":0,"hist":{"boundaries":[0],"counts":[0]}},
            "lastLogAt":1,"restored":1788432607839.9048}"#,
        )
        .unwrap();
        assert!(num.restored, "时间戳 >0 → 已恢复");
        let zero = parse_kb_stats(
            r#"{"totals":{"hits":0,"misses":0,"empty":0,"errs":0,"skipped":0,
            "searches":0,"avgMs":0,"hist":{"boundaries":[0],"counts":[0]}},
            "window":{"hits":0,"misses":0,"empty":0,"errs":0,"skipped":0,
            "searches":0,"avgMs":0,"hist":{"boundaries":[0],"counts":[0]}},
            "lastLogAt":1,"restored":0}"#,
        )
        .unwrap();
        assert!(!zero.restored);
        let boolean = parse_kb_stats(
            r#"{"totals":{"hits":0,"misses":0,"empty":0,"errs":0,"skipped":0,
            "searches":0,"avgMs":0,"hist":{"boundaries":[0],"counts":[0]}},
            "window":{"hits":0,"misses":0,"empty":0,"errs":0,"skipped":0,
            "searches":0,"avgMs":0,"hist":{"boundaries":[0],"counts":[0]}},
            "lastLogAt":1,"restored":true}"#,
        )
        .unwrap();
        assert!(boolean.restored);
    }

    #[test]
    fn error_classification_retryable_vs_user_facing() {
        assert!(MonitorError::Transport("eof".into()).is_retryable());
        assert!(!MonitorError::Http {
            status: 404,
            endpoint: "/agents".into(),
            detail: String::new(),
        }
        .is_retryable());
        assert!(MonitorError::Http {
            status: 500,
            endpoint: "/agents".into(),
            detail: String::new(),
        }
        .is_retryable());
        assert!(!MonitorError::Business {
            error_code: "TIMEOUT".into(),
            message: "x".into(),
        }
        .is_retryable());
        assert_eq!(
            MonitorError::Transport("x".into()).class(),
            ErrorClass::Retryable
        );
        assert_eq!(
            MonitorError::Json("x".into()).class(),
            ErrorClass::UserFacing
        );
    }

    #[test]
    fn error_detail_truncates_to_200_chars() {
        let long = "x".repeat(500);
        let d = error_detail(&long);
        assert_eq!(d.chars().count(), 200);
    }
}
