//! api layer aggregation: DshClient (auth + unary + mux connection).
//!
//! Protocol boundary (Notes/03): all Remote API wrappers are centralized in
//! this layer; no scattered hardcoding. Reconnect orchestration is driven by
//! the single app-layer orchestrator (this layer only provides the connect
//! primitive).

pub mod approval;
pub mod attachment;
pub mod auth;
pub mod envelope;
pub mod monitor;
pub mod mux;
pub mod session;
pub mod types;
pub mod workspace;

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

pub use auth::AuthSession;
pub use envelope::{Backoff, ClientError, ErrorClass};
pub use mux::{Mux, StreamHandle};

/// Remote client: reqwest (HTTP unary, in-memory cookie jar) + mux (WS
/// streams).
pub struct DshClient {
    pub http: reqwest::Client,
    pub base: String,
    pub auth: AuthSession,
}

impl DshClient {
    /// Connect + authenticate (ADR-001 remote client; does not spawn a backend
    /// process).
    pub async fn connect(base: &str, token: &str) -> Result<Self, ClientError> {
        // cookie_store: the auth cookie lives only in memory; never written to
        // disk.
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ClientError::Http(format!("HTTP 客户端构建失败: {e}")))?;
        let auth = auth::authenticate(&http, base, token).await?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            auth,
        })
    }

    /// mux WS address: `ws://{host}/api/remote.mux` (HTTP→WS scheme conversion,
    /// loopback only).
    pub fn mux_url(&self) -> Result<String, ClientError> {
        let ws_base = if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            return Err(ClientError::Protocol(format!(
                "server.url 缺少 http(s):// scheme: {}",
                self.base
            )));
        };
        Ok(format!("{ws_base}/api/remote.mux"))
    }

    /// Open a mux connection (handshake with the auth cookie).
    pub async fn open_mux(&self) -> Result<Mux, ClientError> {
        Mux::connect(&self.mux_url()?, &self.auth.cookie_header()).await
    }
}

/// unary call: `POST {base}/api/{method}` envelope, verify the rpcId echo,
/// classify errors.
pub async fn unary(
    http: &reqwest::Client,
    base: &str,
    method: &str,
    args: Value,
) -> Result<Value, ClientError> {
    let rpc_id = format!("dshtui-{}-{}", std::process::id(), {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        SEQ.fetch_add(1, Ordering::Relaxed)
    });
    let body = envelope::ClientRequest::new(rpc_id.clone(), method, args);
    let url = format!("{}/api/{method}", base.trim_end_matches('/'));

    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| ClientError::Transport(format!("{method} 请求失败（{url}）: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ClientError::Http(format!(
            "{method} HTTP {status}（{url}）"
        )));
    }
    let resp_body: envelope::ServerResponse = resp
        .json()
        .await
        .map_err(|e| ClientError::Envelope(format!("{method} 响应解析失败: {e}")))?;
    if resp_body.rpc_id != rpc_id {
        return Err(ClientError::RpcIdMismatch {
            expected: rpc_id,
            actual: resp_body.rpc_id,
        });
    }
    if resp_body.result.ok {
        Ok(resp_body.result.value.unwrap_or(Value::Null))
    } else {
        match resp_body.result.error {
            Some(e) => Err(envelope::remote_error(&e)),
            None => Err(ClientError::Protocol(format!(
                "{method} 返回 ok=false 但缺少 error"
            ))),
        }
    }
}

/// unary with an explicit deadline (REQ-003: search/approval replies must not
/// hang forever; the baseline `unary` has no timeout of its own). A timeout
/// surfaces as a Transport-class error so the existing reconnect/error
/// classification applies.
pub async fn unary_with_timeout(
    http: &reqwest::Client,
    base: &str,
    method: &str,
    args: Value,
    timeout: std::time::Duration,
) -> Result<Value, ClientError> {
    match tokio::time::timeout(timeout, unary(http, base, method, args)).await {
        Ok(res) => res,
        Err(_elapsed) => Err(ClientError::Transport(format!(
            "{method} 请求超时（>{timeout:?}）"
        ))),
    }
}
