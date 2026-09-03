//! api 层聚合：DshClient（认证 + unary + mux 连接）。
//!
//! 协议边界（Notes/03）：所有 Remote API 封装集中在本层，禁止散落硬编码；
//! 重连编排由 app 层单一 orchestrator 驱动（本层只提供 connect 原语）。

pub mod auth;
pub mod envelope;
pub mod mux;
pub mod session;
pub mod types;
pub mod workspace;

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

pub use auth::AuthSession;
pub use envelope::{Backoff, ClientError, ErrorClass};
pub use mux::{Mux, StreamHandle};

/// 远端客户端：reqwest（HTTP unary，cookie jar 仅内存）+ mux（WS 流）。
pub struct DshClient {
    pub http: reqwest::Client,
    pub base: String,
    pub auth: AuthSession,
    rpc_seq: AtomicU64,
}

impl DshClient {
    /// 连接 + 认证（ADR-001 远端客户端；不拉起后端进程）。
    pub async fn connect(base: &str, token: &str) -> Result<Self, ClientError> {
        // cookie_store：认证 cookie 只存内存；不写磁盘。
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ClientError::Http(format!("HTTP 客户端构建失败: {e}")))?;
        let auth = auth::authenticate(&http, base, token).await?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            auth,
            rpc_seq: AtomicU64::new(1),
        })
    }

    /// mux WS 地址：`ws://{host}/api/remote.mux`（HTTP→WS scheme 转换，仅 loopback）。
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

    /// 打开 mux 连接（携带认证 cookie 握手）。
    pub async fn open_mux(&self) -> Result<Mux, ClientError> {
        Mux::connect(&self.mux_url()?, &self.auth.cookie_header()).await
    }

    /// 生成 rpcId（计数器 + pid 前缀，进程内唯一）。
    pub fn next_rpc_id(&self) -> String {
        let n = self.rpc_seq.fetch_add(1, Ordering::Relaxed);
        format!("dshtui-{}-{}", std::process::id(), n)
    }
}

/// unary 调用：`POST {base}/api/{method}` envelope，校验 rpcId 回显，分类错误。
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
