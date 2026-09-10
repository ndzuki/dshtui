//! WebSocket multiplexing for `/api/remote.mux` (Notes/03 §2.2).
//!
//! Design notes (from the Step 2 Prototype validation):
//! - `WebSocketStream` is not `Clone`: multiple stream responders must share the
//!   write half through `Arc<Mutex<SplitSink>>`; this implementation uses a
//!   "single read loop for dispatch + shared write lock";
//! - `cancel` is best-effort: after a cancel the client **drops** that stream's
//!   in-flight frames (removal from the registry isolates it; other streams are
//!   unaffected);
//! - On disconnect a `Transport` error is broadcast to all active streams, and
//!   the upper orchestrator reconnects uniformly (10s exponential backoff cap).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex};
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
///
/// Key = client-minted stream id **string**。官方 0.1.2-rc.1 的
/// `stream-server.js` `validId()` 要求 `streamId` 为非空字符串（官方 client 用
/// `randomUUID()`）；数字会被 `parseRemoteStreamClientMessage` 拒绝并关闭整个
/// mux（Step E live 实证：数字 streamId → `invalid Remote stream request` +
/// socket 断开；字符串 → item 帧正常回流）。mock 曾用数字 id 且不校验，
/// 掩盖了该 wire 漂移。
type StreamMap = HashMap<String, mpsc::Sender<Result<Value, ClientError>>>;

/// Receive end of one mux stream.
pub struct StreamHandle {
    pub id: String,
    rx: mpsc::Receiver<Result<Value, ClientError>>,
}

impl StreamHandle {
    /// Next frame: `Some(Ok(value))`=item; `Some(Err(e))`=stream error;
    /// `None`=end / connection closed / removed by cancel.
    pub async fn next(&mut self) -> Option<Result<Value, ClientError>> {
        self.rx.recv().await
    }
}

/// A connected mux: open/cancel streams, plus a server-push bypass channel
/// (REQ-003: forwarded `approval/request` waterfall events arrive without a
/// streamId).
pub struct Mux {
    sink: Arc<Mutex<Sink>>,
    streams: Arc<Mutex<StreamMap>>,
    next_id: AtomicU64,
    push_tx: broadcast::Sender<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    stream_id: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    error: Option<super::envelope::RpcError>,
}

impl Mux {
    /// Connect to `ws://{host}/api/remote.mux`, completing the handshake with
    /// the auth cookie.
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

    /// Construct from an already-connected socket (test/reuse path).
    pub fn from_socket(ws: WsStream) -> Self {
        let (sink, mut stream) = ws.split();
        let streams: Arc<Mutex<StreamMap>> = Arc::new(Mutex::new(HashMap::new()));
        let sink = Arc::new(Mutex::new(sink));
        let (push_tx, _) = broadcast::channel(64);
        let mux = Self {
            sink,
            streams: streams.clone(),
            next_id: AtomicU64::new(1),
            push_tx,
        };

        // Single read loop: item → matching stream channel; end → close the
        // channel; error → error frame; anything else (server-pushed frames
        // without a streamId, REQ-003 approval/request) → push bypass channel.
        let streams2 = streams.clone();
        let push_tx2 = mux.push_tx.clone();
        tokio::spawn(async move {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(t))) => {
                        let raw: Value = match serde_json::from_str(&t) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(error = %e, "mux 收到不可解析帧，跳过");
                                continue;
                            }
                        };
                        let frame: ServerFrame = match serde_json::from_value(raw.clone()) {
                            Ok(f) => f,
                            Err(e) => {
                                tracing::warn!(error = %e, "mux 帧形状异常，跳过");
                                continue;
                            }
                        };
                        let mut guard = streams2.lock().await;
                        match (frame.kind.as_str(), frame.stream_id.as_deref()) {
                            ("item", Some(id)) => {
                                if let Some(tx) = guard.get(id) {
                                    if tx
                                        .send(Ok(frame.value.unwrap_or(Value::Null)))
                                        .await
                                        .is_err()
                                    {
                                        guard.remove(id);
                                    }
                                }
                            }
                            ("end", Some(id)) => {
                                guard.remove(id); // drop sender → recv None
                            }
                            ("error", Some(id)) => {
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
                                if let Some(tx) = guard.remove(id) {
                                    let _ = tx.send(Err(e)).await;
                                }
                            }
                            _ => {
                                // Server-pushed frames (no streamId, unknown
                                // kinds, or orphaned stream routing): whole
                                // raw JSON goes to the push bypass. A dropped
                                // broadcast (no subscribers) is expected and
                                // harmless.
                                drop(guard);
                                let _ = push_tx2.send(raw);
                            }
                        }
                    }
                    Some(Ok(_)) => {} // ping/pong/binary ignored
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "mux 连接读错误，广播断开");
                        break;
                    }
                    None => break,
                }
            }
            // Connection closed: broadcast a Transport error to all active
            // streams (the upper layer reconnects uniformly). The push channel
            // dies with the mux (all senders dropped → subscribers see Closed).
            let mut guard = streams2.lock().await;
            let ids = guard.keys().cloned().collect::<Vec<_>>();
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

    /// Subscribe to server-pushed frames (REQ-003 approval bypass). The
    /// subscription dies when the mux is dropped or the connection closes,
    /// so reader tasks need no generation guard of their own.
    pub fn subscribe_push(&self) -> broadcast::Receiver<Value> {
        self.push_tx.subscribe()
    }

    /// Open a stream (open frame), returning the receive handle.
    ///
    /// streamId 客户端铸币为**非空字符串**（官方 0.1.2-rc.1 协议要求，官方
    /// client 用 `randomUUID()`；数字会被服务端 `validId` 拒绝并断开整个
    /// mux——Step E live 实证）。内部计数器只保证单 mux 内唯一，前缀
    /// `dshtui-` + 计数即可（无需 uuid crate）。
    pub async fn open_stream(
        &self,
        endpoint: &str,
        args: Value,
    ) -> Result<StreamHandle, ClientError> {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("dshtui-{n}");
        let (tx, rx) = mpsc::channel(256);
        self.streams.lock().await.insert(id.clone(), tx);
        let frame = serde_json::json!({
            "type": "open",
            "streamId": id,
            "endpoint": endpoint,
            "payload": {"args": args}
        });
        if let Err(e) = self.send(&frame).await {
            // Open send failed: roll back the registration and fail fast
            // (never fail silently).
            self.streams.lock().await.remove(&id);
            return Err(ClientError::Transport(format!(
                "open {endpoint} 发送失败: {e}"
            )));
        }
        tracing::debug!(stream_id = %id, %endpoint, "mux stream 已打开");
        Ok(StreamHandle { id, rx })
    }

    /// Cancel a stream (cancel frame + registry removal; later in-flight
    /// frames are dropped).
    pub async fn cancel_stream(&self, id: &str) {
        let frame = serde_json::json!({"type": "cancel", "streamId": id});
        let _ = self.send(&frame).await;
        self.streams.lock().await.remove(id);
        tracing::debug!(stream_id = %id, "mux stream 已取消");
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
            serde_json::from_str(r#"{"type":"item","streamId":"dshtui-3","value":{"hello":1}}"#)
                .unwrap();
        assert_eq!(f.kind, "item");
        assert_eq!(f.stream_id.as_deref(), Some("dshtui-3"));
        assert_eq!(f.value.unwrap()["hello"], 1);

        let f: ServerFrame =
            serde_json::from_str(r#"{"type":"end","streamId":"dshtui-3"}"#).unwrap();
        assert!(f.value.is_none());

        let f: ServerFrame = serde_json::from_str(
            r#"{"type":"error","streamId":"dshtui-3","error":{"code":"PERMISSION_DENIED","message":"x"}}"#,
        )
        .unwrap();
        assert_eq!(f.error.unwrap().code, "PERMISSION_DENIED");
    }

    #[test]
    fn server_frame_without_stream_id_defaults_to_none() {
        // Server-pushed frames (approval/request) carry no streamId; serde
        // default gives None, which the read loop routes to the push bypass.
        let f: ServerFrame =
            serde_json::from_str(r#"{"type":"approval/request","clientId":"c1"}"#).unwrap();
        assert_eq!(f.kind, "approval/request");
        assert_eq!(f.stream_id, None);
    }
}
