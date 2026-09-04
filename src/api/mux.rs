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

/// Receive end of one mux stream.
pub struct StreamHandle {
    pub id: u64,
    rx: mpsc::Receiver<Result<Value, ClientError>>,
}

impl StreamHandle {
    /// Next frame: `Some(Ok(value))`=item; `Some(Err(e))`=stream error;
    /// `None`=end / connection closed / removed by cancel.
    pub async fn next(&mut self) -> Option<Result<Value, ClientError>> {
        self.rx.recv().await
    }
}

/// A connected mux: open/cancel streams.
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
        let mux = Self {
            sink,
            streams: streams.clone(),
            next_id: AtomicU64::new(1),
        };

        // Single read loop: item → matching stream channel; end → close the
        // channel; error → error frame.
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
                                // Tolerate unknown frame types (Notes/03 §7
                                // compatibility action); log and skip.
                                tracing::warn!(kind = %frame.kind, "mux 未知帧类型，跳过");
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
            // streams (the upper layer reconnects uniformly).
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

    /// Open a stream (open frame), returning the receive handle.
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
            // Open send failed: roll back the registration and fail fast
            // (never fail silently).
            self.streams.lock().await.remove(&id);
            return Err(ClientError::Transport(format!(
                "open {endpoint} 发送失败: {e}"
            )));
        }
        tracing::debug!(stream_id = id, %endpoint, "mux stream 已打开");
        Ok(StreamHandle { id, rx })
    }

    /// Cancel a stream (cancel frame + registry removal; later in-flight
    /// frames are dropped).
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
