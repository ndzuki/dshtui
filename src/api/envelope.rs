//! Unary 信封（Notes/03 §2.1）与错误分类（§8）。
//!
//! - 错误按 `error.code` 判别（不是 message 文本）；`PERMISSION_DENIED` 不自动重试；
//! - 网络断开 → 指数退避重连（上限 10s）；
//! - 所有错误统一走 tracing（脱敏，不刷屏终端）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------- 信封 ----------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientRequest {
    #[serde(rename = "type")]
    pub kind: &'static str, // "client-request"
    pub rpc_id: String,
    pub method: String,
    pub payload: Value,
}

impl ClientRequest {
    /// 构造 `{"type":"client-request","rpcId":..,"method":..,"payload":{"args":{..}}}`。
    pub fn new(rpc_id: impl Into<String>, method: impl Into<String>, args: Value) -> Self {
        Self {
            kind: "client-request",
            rpc_id: rpc_id.into(),
            method: method.into(),
            payload: serde_json::json!({ "args": args }),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerResponse {
    #[serde(rename = "type")]
    pub kind: String,
    pub rpc_id: String,
    pub result: RpcResult,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcResult {
    pub ok: bool,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub code: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub details: Option<Value>,
}

// ---------- 错误分类（Notes/03 §8） ----------

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("认证失败: {0}")]
    Auth(String),
    #[error("HTTP 请求失败: {0}")]
    Http(String),
    #[error("信封解析失败: {0}")]
    Envelope(String),
    #[error("rpcId 不匹配: 期望 {expected} 收到 {actual}")]
    RpcIdMismatch { expected: String, actual: String },
    #[error("服务端错误 [{code}]: {message}")]
    Remote {
        code: String,
        message: String,
        class: ErrorClass,
    },
    #[error("流错误 [{code}]: {message}")]
    Stream {
        code: String,
        message: String,
        class: ErrorClass,
    },
    #[error("网络传输失败: {0}")]
    Transport(String),
    #[error("协议形状不符合预期: {0}")]
    Protocol(String),
}

/// 错误类别：决定是否自动重试（Notes/03 §8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// 网络断开/传输层失败 → 指数退避重连。
    Retryable,
    /// 权限/审批 → 提示用户，绝不自动重试。
    PermissionDenied,
    /// 其它服务端业务错误 → 提示用户，不自动重试。
    UserFacing,
}

impl ErrorClass {
    /// 按 `error.code` 分类（不是 message 文本，Notes/03 §8）。
    pub fn from_code(code: &str) -> ErrorClass {
        if code == "PERMISSION_DENIED" || code.contains("permission") {
            ErrorClass::PermissionDenied
        } else {
            ErrorClass::UserFacing
        }
    }
}

impl ClientError {
    pub fn classify(code: &str) -> ErrorClass {
        ErrorClass::from_code(code)
    }

    pub fn class(&self) -> ErrorClass {
        match self {
            ClientError::Transport(_) => ErrorClass::Retryable,
            ClientError::Remote { class, .. } | ClientError::Stream { class, .. } => *class,
            ClientError::Auth(_) | ClientError::Http(_) | ClientError::Envelope(_) => {
                ErrorClass::UserFacing
            }
            ClientError::RpcIdMismatch { .. } | ClientError::Protocol(_) => ErrorClass::UserFacing,
        }
    }
}

/// 从 `{"ok":false,"error":{code,message,details}}` 构造错误。
pub fn remote_error(e: &RpcError) -> ClientError {
    ClientError::Remote {
        code: e.code.clone(),
        message: e.message.clone().unwrap_or_else(|| "无消息".to_string()),
        class: ClientError::classify(&e.code),
    }
}

// ---------- 指数退避（上限 10s） ----------

#[derive(Debug, Clone)]
pub struct Backoff {
    attempt: u32,
    base_ms: u64,
    cap_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(500, 10_000)
    }
}

impl Backoff {
    pub fn new(base_ms: u64, cap_ms: u64) -> Self {
        Self {
            attempt: 0,
            base_ms,
            cap_ms,
        }
    }

    /// 下一次退避延迟（2^n * base，封顶 cap），返回后 attempt+1。
    pub fn next_delay_ms(&mut self) -> u64 {
        let exp = self.attempt.min(16);
        let ms = self.base_ms.saturating_mul(1u64 << exp).min(self.cap_ms);
        self.attempt += 1;
        ms
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn attempts(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_request_shape() {
        let req = ClientRequest::new("r1", "session/list", serde_json::json!({"cursor": null}));
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["type"], "client-request");
        assert_eq!(v["rpcId"], "r1");
        assert_eq!(v["method"], "session/list");
        assert_eq!(v["payload"]["args"]["cursor"], Value::Null);
    }

    #[test]
    fn envelope_response_ok_and_error() {
        let ok: ServerResponse = serde_json::from_str(
            r#"{"type":"server-response","rpcId":"r1","result":{"ok":true,"value":{"items":[]}}}"#,
        )
        .unwrap();
        assert_eq!(ok.kind, "server-response");
        assert!(ok.result.ok);
        assert!(ok.result.error.is_none());

        let err: ServerResponse = serde_json::from_str(
            r#"{"type":"server-response","rpcId":"r1","result":{"ok":false,"error":{"code":"session/agent-busy","message":"忙"}}}"#,
        )
        .unwrap();
        assert!(!err.result.ok);
        let e = remote_error(err.result.error.as_ref().unwrap());
        assert!(matches!(e, ClientError::Remote { ref code, .. } if code == "session/agent-busy"));
        assert_eq!(e.class(), ErrorClass::UserFacing);
    }

    #[test]
    fn error_classification_by_code_not_message() {
        // 权限 → 不自动重试（Notes/03 §8）。
        assert_eq!(
            ClientError::classify("PERMISSION_DENIED"),
            ErrorClass::PermissionDenied
        );
        assert_eq!(
            ClientError::classify("gateway/permission-required"),
            ErrorClass::PermissionDenied
        );
        // 其它服务端错误 → 提示用户，不重试。
        assert_eq!(
            ClientError::classify("gateway/bad-request"),
            ErrorClass::UserFacing
        );
        assert_eq!(
            ClientError::classify("FEATURE_UNAVAILABLE"),
            ErrorClass::UserFacing
        );
        // 传输层 → 可重试。
        assert_eq!(
            ClientError::Transport("eof".into()).class(),
            ErrorClass::Retryable
        );
    }

    #[test]
    fn backoff_caps_at_10s() {
        let mut b = Backoff::new(500, 10_000);
        let mut delays = Vec::new();
        for _ in 0..12 {
            delays.push(b.next_delay_ms());
        }
        // 500, 1000, 2000, 4000, 8000, 10000, 10000, ...
        assert_eq!(delays[0], 500);
        assert_eq!(delays[1], 1000);
        assert_eq!(delays[4], 8000);
        assert_eq!(delays[5], 10_000);
        assert_eq!(delays[11], 10_000);
        assert!(delays.iter().all(|d| *d <= 10_000), "上限必须为 10s");
        b.reset();
        assert_eq!(b.attempts(), 0);
        assert_eq!(b.next_delay_ms(), 500);
    }
}
