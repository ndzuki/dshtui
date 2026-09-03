//! WebSocket 多路复用 `/api/remote.mux`（Notes/03 §2.2）。
//!
//! 设计要点（来自 Step 2 Prototype 验证结论）：
//! - `WebSocketStream` 不可 Clone：多 stream responder 共享写半必须
//!   `Arc<Mutex<SplitSink>>`，本实现为「单读循环分发 + 共享写锁」；
//! - `cancel` 是尽力而为：客户端在 cancel 后**丢弃**该 stream 的在途帧
//!   （从注册表摘除即隔离，不影响其他 stream）；
//! - 连接断开时向所有活跃 stream 广播 `Transport` 错误，由上层 orchestrator
//!   统一重连（上限 10s 指数退避）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

use super::envelope::{ClientError, ErrorClass};

type Sink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// Fully-negotiated WebSocket socket type (factored out for readability).
type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Per-stream response channel registry.
type StreamMap = HashMap<u64, mpsc::Sender<Result<Value, ClientError>>>;

/// 一条 mux stream 的接收端。
pub struct StreamHandle {
    pub id: u64,
    rx: mpsc::Receiver<Result<Value, ClientError>>,
}

impl StreamHandle {
    /// 下一帧：`Some(Ok(value))`=item；`Some(Err(e))`=stream error；
    /// `None`=end / 连接关闭 / cancel 摘除。
    pub async fn next(&mut self) -> Option<Result<Value, ClientError>> {
        self.rx.recv().await
    }
}

/// 已连接的 mux：打开/取消 stream。
pub struct Mux {
    sink: Arc<Mutex<Sink>>,
    streams: Arc<Mutex<StreamMap>>,
    next_id: AtomicU64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    stream_id: u64,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    error: Option<super::envelope::RpcError>,
}

impl Mux {
    /// 连接 `ws://{host}/api/remote.mux`，携带认证 cookie 完成握手。
    pub async fn connect(ws_url: &str, cookie_header: &str) -> Result<Self, ClientError> {
        let mut req = ws_url
            .into_client_request()
            .map_err(|e| ClientError::Protocol(format!("WS 请求构造失败: {e}")))?;
        req.headers_mut().insert(
            http::header::COOKIE,
            cookie_header
                .parse()
                .map_err(|e| ClientError::Protocol(format!("Cookie header 无效: {e}")))?,
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|e| ClientError::Transport(format!("WS 连接失败（{ws_url}）: {e}")))?;
        tracing::debug!(%ws_url, "mux 已连接");
        Ok(Self::from_socket(ws))
    }

    /// 从已建立连接的 socket 构造（测试/复用路径）。
    pub fn from_socket(ws: WsStream) -> Self {
        let (sink, mut stream) = ws.split();
        let streams: Arc<Mutex<StreamMap>> = Arc::new(Mutex::new(HashMap::new()));
        let sink = Arc::new(Mutex::new(sink));
        let mux = Self {
            sink,
            streams: streams.clone(),
            next_id: AtomicU64::new(1),
        };

        // 单读循环：item → 对应 stream 通道；end → 关闭通道；error → 错误帧。
        let streams2 = streams.clone();
        tokio::spawn(async move {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(t))) => {
                        let frame: ServerFrame = match serde_json::from_str(&t) {
                            Ok(f) => f,
                            Err(e) => {
                                tracing::warn!(error = %e, "mux 收到不可解析帧，跳过");
                                continue;
                            }
                        };
                        let mut guard = streams2.lock().await;
                        match frame.kind.as_str() {
                            "item" => {
                                if let Some(tx) = guard.get(&frame.stream_id) {
                                    if tx
                                        .send(Ok(frame.value.unwrap_or(Value::Null)))
                                        .await
                                        .is_err()
                                    {
                                        guard.remove(&frame.stream_id);
                                    }
                                }
                            }
                            "end" => {
                                guard.remove(&frame.stream_id); // drop sender → recv None
                            }
                            "error" => {
                                let err = frame.error.unwrap_or(super::envelope::RpcError {
                                    code: "stream/error".into(),
                                    message: None,
                                    details: None,
                                });
                                let e = ClientError::Stream {
                                    code: err.code.clone(),
                                    message: err.message.clone().unwrap_or_else(|| "流错误".into()),
                                    class: ErrorClass::from_code(&err.code),
                                };
                                if let Some(tx) = guard.remove(&frame.stream_id) {
                                    let _ = tx.send(Err(e)).await;
                                }
                            }
                            _ => {
                                // 未知帧类型容忍（Notes/03 §7 兼容动作），计入日志。
                                tracing::warn!(kind = %frame.kind, "mux 未知帧类型，跳过");
                            }
                        }
                    }
                    Some(Ok(_)) => {} // ping/pong/binary 忽略
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "mux 连接读错误，广播断开");
                        break;
                    }
                    None => break,
                }
            }
            // 连接断开：所有活跃 stream 广播 Transport 错误（上层统一重连）。
            let mut guard = streams2.lock().await;
            let ids = guard.keys().copied().collect::<Vec<_>>();
            for id in ids {
                if let Some(tx) = guard.remove(&id) {
                    let _ = tx
                        .send(Err(ClientError::Transport("mux 连接已断开".into())))
                        .await;
                }
            }
        });
        mux
    }

    /// 打开一个 stream（open 帧），返回接收 handle。
    pub async fn open_stream(
        &self,
        endpoint: &str,
        args: Value,
    ) -> Result<StreamHandle, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(256);
        self.streams.lock().await.insert(id, tx);
        let frame = serde_json::json!({
            "type": "open",
            "streamId": id,
            "endpoint": endpoint,
            "payload": {"args": args}
        });
        if let Err(e) = self.send(&frame).await {
            // 打开发送失败：回滚注册并报错（fail-fast，不静默）。
            self.streams.lock().await.remove(&id);
            return Err(ClientError::Transport(format!(
                "open {endpoint} 发送失败: {e}"
            )));
        }
        tracing::debug!(stream_id = id, %endpoint, "mux stream 已打开");
        Ok(StreamHandle { id, rx })
    }

    /// 取消 stream（cancel 帧 + 摘除注册表；后续在途帧被丢弃）。
    pub async fn cancel_stream(&self, id: u64) {
        let frame = serde_json::json!({"type": "cancel", "streamId": id});
        let _ = self.send(&frame).await;
        self.streams.lock().await.remove(&id);
        tracing::debug!(stream_id = id, "mux stream 已取消");
    }

    async fn send(&self, frame: &Value) -> Result<(), String> {
        let mut guard = self.sink.lock().await;
        guard
            .send(Message::Text(frame.to_string()))
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_frame_parses_item_end_error() {
        let f: ServerFrame =
            serde_json::from_str(r#"{"type":"item","streamId":3,"value":{"hello":1}}"#).unwrap();
        assert_eq!(f.kind, "item");
        assert_eq!(f.stream_id, 3);
        assert_eq!(f.value.unwrap()["hello"], 1);

        let f: ServerFrame = serde_json::from_str(r#"{"type":"end","streamId":3}"#).unwrap();
        assert!(f.value.is_none());

        let f: ServerFrame = serde_json::from_str(
            r#"{"type":"error","streamId":3,"error":{"code":"PERMISSION_DENIED","message":"x"}}"#,
        )
        .unwrap();
        assert_eq!(f.error.unwrap().code, "PERMISSION_DENIED");
    }
}
