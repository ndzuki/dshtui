//! THROWAWAY PROTOTYPE（Step 2 Gate）——验证 Remote API 协议假设。
//! 问题：cookie jar 认证 + unary envelope + 单 WS 多 stream mux 能否按
//! Notes/03 §1/§2 在当前依赖集（reqwest 0.12 / tokio-tungstenite 0.24）实现？
//! 运行：cargo run --example proto_step2 —— 全部断言通过打印 PASS，失败打印 FAIL。
//! 本文件不会进入 commit（验证后删除）。

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

const HTTP_PORT: u16 = 38211;
const WS_PORT: u16 = 38212;

#[tokio::main]
async fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).finish(),
    )
    .ok();

    let http = tokio::spawn(mock_http(HTTP_PORT));
    let ws = tokio::spawn(mock_ws(WS_PORT));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    match run_client().await {
        Ok(()) => println!("PROTO PASS: auth→list、双 stream 同 WS、cancel 隔离全部成立"),
        Err(e) => {
            println!("PROTO FAIL: {e}");
            std::process::exit(1);
        }
    }
    http.abort();
    ws.abort();
}

// ---------- mock HTTP：GET /?token → 303+cookie；POST /api/session/list → envelope ----------

async fn mock_http(port: u16) {
    let l = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    loop {
        let (mut s, _) = l.accept().await.unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            let req = String::from_utf8_lossy(&buf[..n]);
            let line = req.lines().next().unwrap_or("");
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("");
            let (path, _query) = target.split_once('?').unwrap_or((target, ""));
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
            let body = &buf[body_start..n];

            let resp = if method == "GET" && path == "/" {
                "HTTP/1.1 303 See Other\r\nLocation: /\r\nSet-Cookie: dsh-auth-0=v1.proto.sig; HttpOnly; SameSite=Strict; Max-Age=15552000\r\nContent-Length: 0\r\n\r\n".to_string()
            } else if method == "POST" && path == "/api/session/list" {
                let body = String::from_utf8_lossy(body);
                let req: serde_json::Value = serde_json::from_str(body.trim_end_matches('\0')).unwrap();
                let rpc_id = req["rpcId"].as_str().unwrap_or("").to_string();
                let list = serde_json::json!({
                    "type": "server-response",
                    "rpcId": rpc_id,
                    "result": {"ok": true, "value": {"items": [{"id": "sess-1", "title": "t", "updatedAtMs": 1}], "nextCursor": null}}
                });
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    list.to_string().len(),
                    list
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
            };
            let _ = s.write_all(resp.as_bytes()).await;
            let _ = s.shutdown().await;
        });
    }
}

// ---------- mock WS：/api/remote.mux，open/cancel，item/end ----------

type CancelMap = Arc<Mutex<HashMap<u64, tokio::sync::mpsc::Sender<()>>>>;

async fn mock_ws(port: u16) {
    let l = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    loop {
        let (s, _) = l.accept().await.unwrap();
        let cancels: CancelMap = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(handle_ws_conn(s, cancels));
    }
}

async fn handle_ws_conn(s: TcpStream, cancels: CancelMap) {
    let ws = match tokio_tungstenite::accept_async(s).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("ws accept err: {e}");
            return;
        }
    };
    // 关键发现（回写真实 mux 设计）：WebSocketStream 不可 Clone，多 stream
    // responder 共享写半必须走 Arc<Mutex<SplitSink>>（或单写任务 + mpsc 汇聚）。
    let (sink, mut stream) = ws.split();
    let sink = Arc::new(Mutex::new(sink));
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => {
                let frame: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let stype = frame["type"].as_str().unwrap_or("");
                let sid = frame["streamId"].as_u64().unwrap_or(0);
                match stype {
                    "open" => {
                        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
                        cancels.lock().await.insert(sid, tx);
                        let sink = sink.clone();
                        tokio::spawn(async move {
                            // 每条 stream 发 2 条 item 然后 end；cancel 在 item 间隙
                            // （30ms）抢占，验证 cancel 生效 + 允许最多 1 条在途帧。
                            let mut canceled = false;
                            for i in 0..2 {
                                let item = serde_json::json!({
                                    "type": "item",
                                    "streamId": sid,
                                    "value": {"n": i, "endpoint": frame["endpoint"]}
                                })
                                .to_string();
                                // 注意：tokio::select! 内不能直接 await Mutex guard（临时值
                                // E0716）——必须包成 async block 持有 guard 到发送完成。
                                tokio::select! {
                                    _ = rx.recv() => { canceled = true; break; }
                                    r = async {
                                        let mut guard = sink.lock().await;
                                        guard.send(tokio_tungstenite::tungstenite::Message::Text(item)).await
                                    } => {
                                        if r.is_err() { break; }
                                    }
                                }
                                // cancel 抢占窗口：真实 mux 中 cancel 是尽力而为，
                                // 客户端必须容忍取消后的在途帧（丢弃即可）。
                                tokio::select! {
                                    _ = rx.recv() => { canceled = true; break; }
                                    _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
                                }
                            }
                            if !canceled {
                                let end = serde_json::json!({"type": "end", "streamId": sid}).to_string();
                                let _ = sink
                                    .lock()
                                    .await
                                    .send(tokio_tungstenite::tungstenite::Message::Text(end))
                                    .await;
                            }
                        });
                    }
                    "cancel" => {
                        if let Some(tx) = cancels.lock().await.remove(&sid) {
                            let _ = tx.send(()).await;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

// ---------- 客户端：按 Notes/03 §1/§2 最小实现 ----------

#[derive(Debug)]
struct ProtoErr(String);
impl std::fmt::Display for ProtoErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ProtoErr {}
type R<T> = Result<T, ProtoErr>;

async fn run_client() -> R<()> {
    // 1) 认证：GET /?token → 303 + cookie（reqwest cookie jar，仅内存）。
    let http = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = http
        .get(format!("http://127.0.0.1:{HTTP_PORT}/?token=tok123"))
        .send()
        .await
        .map_err(|e| ProtoErr(e.to_string()))?;
    assert_eq!(resp.status().as_u16(), 303, "认证必须是 303");
    let cookies = resp.cookies().collect::<Vec<_>>();
    assert!(
        cookies.iter().any(|c| c.name().starts_with("dsh-auth-")),
        "必须收到 dsh-auth-<n> cookie"
    );

    // 2) unary envelope：POST /api/session/list。
    let rpc_id = "rpc-1";
    let req_body = serde_json::json!({
        "type": "client-request",
        "rpcId": rpc_id,
        "method": "session/list",
        "payload": {"args": {}}
    });
    let resp = http
        .post(format!("http://127.0.0.1:{HTTP_PORT}/api/session/list"))
        .json(&req_body)
        .send()
        .await
        .map_err(|e| ProtoErr(e.to_string()))?;
    let env: serde_json::Value = resp.json().await.map_err(|e| ProtoErr(e.to_string()))?;
    assert_eq!(env["type"], "server-response", "信封类型不符");
    assert_eq!(env["rpcId"], rpc_id, "rpcId 必须回显");
    assert_eq!(env["result"]["ok"], true, "result.ok 必须为 true");
    assert!(
        env["result"]["value"]["items"].is_array(),
        "session/list items 必须是数组"
    );

    // 3) WS mux：单连接双 stream + cancel 隔离。
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{WS_PORT}/api/remote.mux"))
        .await
        .map_err(|e| ProtoErr(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();

    let open = |sid: u64, endpoint: &str| {
        serde_json::json!({"type": "open", "streamId": sid, "endpoint": endpoint, "payload": {"args": {}}})
            .to_string()
    };
    sink.send(tokio_tungstenite::tungstenite::Message::Text(open(1, "session/follow".into())))
        .await
        .map_err(|e| ProtoErr(e.to_string()))?;
    sink.send(tokio_tungstenite::tungstenite::Message::Text(open(2, "workspace/follow".into())))
        .await
        .map_err(|e| ProtoErr(e.to_string()))?;
    // cancel stream1：stream2 必须不受影响地收到 item+end。
    sink.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({"type": "cancel", "streamId": 1}).to_string(),
    ))
    .await
    .map_err(|e| ProtoErr(e.to_string()))?;
    // 再开 stream3 验证正常收尾。
    sink.send(tokio_tungstenite::tungstenite::Message::Text(open(3, "session/follow".into())))
        .await
        .map_err(|e| ProtoErr(e.to_string()))?;

    let mut s1_frames = 0u32;
    let mut s1_ended = false;
    let mut s2_frames = 0u32;
    let mut s2_ended = false;
    let mut s3_ended = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !(s2_ended && s3_ended) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), stream.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                let f: serde_json::Value = serde_json::from_str(&t).map_err(|e| ProtoErr(e.to_string()))?;
                let sid = f["streamId"].as_u64().unwrap_or(0);
                match sid {
                    1 => {
                        // cancel 后最多 1 条在途 item，且绝不能收到 end。
                        s1_frames += 1;
                        if f["type"] == "end" {
                            s1_ended = true;
                        }
                    }
                    2 => {
                        s2_frames += 1;
                        if f["type"] == "end" {
                            s2_ended = true;
                        }
                    }
                    3 => {
                        if f["type"] == "end" {
                            s3_ended = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(ProtoErr(format!("WS 帧错误: {e}"))),
            Ok(None) | Err(_) => break,
        }
    }
    assert!(!s1_ended, "cancel 后 stream1 不得收到 end");
    assert!(
        s1_frames <= 1,
        "cancel 后 stream1 只允许 ≤1 条在途帧，实收 {s1_frames} 帧"
    );
    assert!(s2_ended, "stream2 必须收到 end（cancel stream1 不影响 stream2）");
    assert!(s3_ended, "stream3 必须正常收到 end");
    assert!(s2_frames >= 3, "stream2 应收到 ≥2 item + end，实收 {s2_frames} 帧");
    Ok(())
}
