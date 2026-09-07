use std::time::Duration;

use base64::Engine as _;
use dshtui::api::attachment;
use dshtui::api::auth::authenticate;
use dshtui::api::envelope::{remote_error, ClientRequest, ErrorClass, RpcError, ServerResponse};
use dshtui::api::session::{self, cancel, prompt, AcceptedValue};
use dshtui::api::types::{
    AttachmentId, ControlItem, FollowFrame, PromptContentPart, PromptMode, PromptRequest,
    SessionId, SessionRequestId, SessionSeq,
};
use dshtui::api::{ClientError, Mux};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

/// 读一个 HTTP 请求到 body 边界，返回（请求头 + body 原文）。
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        let read = socket.read(&mut buf).await.unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buf[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

async fn write_json_response(socket: &mut tokio::net::TcpStream, resp: Value) {
    let body = resp.to_string();
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

fn queue_prompt_request() -> PromptRequest {
    PromptRequest {
        request_id: SessionRequestId("client-minted-1".into()),
        session_id: SessionId("sess-1".into()),
        mode: PromptMode::Queue,
        content: vec![PromptContentPart::Text {
            text: "你好 draft".into(),
        }],
        client_time_zone: None,
    }
}

/// Read one full HTTP/1.1 request (headers + Content-Length body).
async fn read_http_request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut raw = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&raw[..hdr_end]).to_string();
        let content_len = headers
            .lines()
            .find_map(|l| {
                let (name, value) = l.split_once(':')?;
                if name.trim().eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        if raw.len() >= hdr_end + 4 + content_len {
            return (
                headers,
                raw[hdr_end + 4..hdr_end + 4 + content_len].to_vec(),
            );
        }
    }
    (String::new(), raw)
}

#[test]
fn typed_envelope_round_trips_request_and_response() {
    let request = ClientRequest::new(
        "rpc-7",
        "session/list",
        json!({"cursor": null, "limit": 20}),
    );
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        encoded,
        json!({
            "type": "client-request",
            "rpcId": "rpc-7",
            "method": "session/list",
            "payload": {"args": {"cursor": null, "limit": 20}}
        })
    );

    let response: ServerResponse = serde_json::from_value(json!({
        "type": "server-response",
        "rpcId": "rpc-7",
        "result": {"ok": true, "value": {"items": []}}
    }))
    .unwrap();
    assert_eq!(response.kind, "server-response");
    assert_eq!(response.rpc_id, "rpc-7");
    assert!(response.result.ok);
    assert_eq!(response.result.value.unwrap(), json!({"items": []}));

    let error = RpcError {
        code: "PERMISSION_DENIED".into(),
        message: Some("not allowed".into()),
        details: Some(json!({"scope": "session"})),
    };
    assert_eq!(remote_error(&error).class(), ErrorClass::PermissionDenied);
}

#[test]
fn typed_follow_frame_accepts_snapshot_and_event_shapes() {
    let snapshot: FollowFrame = serde_json::from_value(json!({
        "type": "snapshot",
        "cursor": 41,
        "records": [],
        "hasMore": true,
        "projections": {"running": false}
    }))
    .unwrap();
    match snapshot {
        FollowFrame::Snapshot {
            cursor,
            has_more,
            projections,
            ..
        } => {
            assert_eq!(cursor.map(|v| v.0), Some(41));
            assert_eq!(has_more, Some(true));
            assert_eq!(projections.unwrap()["running"], false);
        }
        FollowFrame::Event { .. } => panic!("expected snapshot"),
    }

    let event: FollowFrame = serde_json::from_value(json!({
        "type": "event",
        "event": {"type": "turn/end", "seq": 42, "requestId": "req-42"}
    }))
    .unwrap();
    assert!(matches!(
        event,
        FollowFrame::Event { event } if event.seq == Some(SessionSeq(42))
            && event.request_id.as_deref() == Some("req-42")
    ));
}

#[tokio::test]
async fn authenticate_extracts_cookie_pair_without_attributes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            let read = socket.read(&mut buf).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        assert!(request.starts_with("GET /?token=secret-token HTTP/1.1"));
        socket
            .write_all(
                b"HTTP/1.1 303 See Other\r\nLocation: /\r\nSet-Cookie: dsh-auth-0=v1.sig; Path=/; HttpOnly\r\nSet-Cookie: sid=abc; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });

    let http = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let session = authenticate(&http, &format!("http://{addr}"), "secret-token")
        .await
        .unwrap();
    assert_eq!(session.cookie_header(), "dsh-auth-0=v1.sig; sid=abc");
    server.await.unwrap();
}

#[tokio::test]
async fn mux_open_stream_routes_item_and_end_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel::<Value>();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let first = ws.next().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
        opened_tx.send(frame.clone()).unwrap();
        let stream_id = frame["streamId"].as_u64().unwrap();

        ws.send(Message::Text(
            json!({"type": "item", "streamId": stream_id, "value": {"answer": 42}}).to_string(),
        ))
        .await
        .unwrap();
        ws.send(Message::Text(
            json!({"type": "end", "streamId": stream_id}).to_string(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    });

    let mux = Mux::connect(&format!("ws://{addr}"), "dsh-auth-0=v1.sig")
        .await
        .unwrap();
    let mut stream = mux
        .open_stream("session/follow", json!({"maxMessages": 2}))
        .await
        .unwrap();

    let opened = tokio::time::timeout(Duration::from_secs(1), opened_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opened["type"], "open");
    assert_eq!(opened["endpoint"], "session/follow");
    assert_eq!(opened["payload"]["args"]["maxMessages"], 2);

    let item = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(item, json!({"answer": 42}));
    assert!(tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .is_none());
    server.await.unwrap();
}

// ---------- REQ-002: session/prompt + typed session/cancel ----------

#[test]
fn prompt_request_serializes_official_camel_case_shape() {
    // 字段名实读（2026-09-04）: requestId/sessionId/mode/content/clientTimeZone。
    let request = queue_prompt_request();
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        encoded,
        json!({
            "requestId": "client-minted-1",
            "sessionId": "sess-1",
            "mode": "queue",
            "content": [{"type": "text", "text": "你好 draft"}],
        })
    );
    // clientTimeZone 省略（V0.1 不上送）。
    assert!(encoded.get("clientTimeZone").is_none());

    // steer 扩展位（V0.2/REQ-003）：同一类型可序列化。
    let steer = serde_json::to_value(PromptRequest {
        mode: PromptMode::Steer,
        ..queue_prompt_request()
    })
    .unwrap();
    assert_eq!(steer["mode"], "steer");
}

#[tokio::test]
async fn prompt_unary_posts_official_args_and_parses_accepted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/session/prompt HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["type"], "client-request");
        assert_eq!(body["method"], "session/prompt");
        let args = &body["payload"]["args"];
        assert_eq!(args["requestId"], "client-minted-1");
        assert_eq!(args["sessionId"], "sess-1");
        assert_eq!(args["mode"], "queue");
        assert_eq!(args["content"][0]["type"], "text");
        assert_eq!(args["content"][0]["text"], "你好 draft");
        assert!(args.get("clientTimeZone").is_none());
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {"accepted": true}},
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let accepted = prompt(&http, &format!("http://{addr}"), &queue_prompt_request())
        .await
        .unwrap();
    assert_eq!(accepted, AcceptedValue { accepted: true });
    server.await.unwrap();
}

#[tokio::test]
async fn prompt_unary_surfaces_remote_error_code_and_class() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": false, "error": {
                    "code": "gateway/bad-request",
                    "message": "非法请求",
                    "details": {"scope": "session"}
                }},
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let err = prompt(&http, &format!("http://{addr}"), &queue_prompt_request())
        .await
        .unwrap_err();
    match err {
        ClientError::Remote {
            code,
            message,
            class,
        } => {
            assert_eq!(code, "gateway/bad-request", "错误码保留");
            assert_eq!(message, "非法请求");
            assert_eq!(class, ErrorClass::UserFacing);
        }
        other => panic!("预期 Remote 错误，得到 {other:?}"),
    }
    server.await.unwrap();
}

#[tokio::test]
async fn cancel_unary_returns_typed_accepted_receipt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/session/cancel HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["method"], "session/cancel");
        assert_eq!(body["payload"]["args"]["sessionId"], "sess-1");
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {"accepted": true}},
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let accepted = cancel(&http, &format!("http://{addr}"), "sess-1")
        .await
        .unwrap();
    assert_eq!(accepted, AcceptedValue { accepted: true });
    server.await.unwrap();
}

#[tokio::test]
async fn cancel_unary_surfaces_rejection_without_panicking() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": false, "error": {
                    "code": "session/agent-busy",
                    "message": "忙",
                }},
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let err = cancel(&http, &format!("http://{addr}"), "sess-1")
        .await
        .unwrap_err();
    match err {
        ClientError::Remote { code, .. } => assert_eq!(code, "session/agent-busy"),
        other => panic!("预期 Remote 错误，得到 {other:?}"),
    }
    server.await.unwrap();
}

#[tokio::test]
async fn accepted_false_receipt_is_a_typed_rejection_ac002_09() {
    // ok:true 但 value.accepted=false（服务端拒绝）：必须成为 typed 失败，
    // 不能被静默当作成功（AC-002-09 被服务端拒绝路径）。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {"accepted": false}},
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let err = cancel(&http, &format!("http://{addr}"), "sess-1")
        .await
        .unwrap_err();
    match err {
        ClientError::Protocol(msg) => assert!(msg.contains("accepted=false"), "msg={msg}"),
        other => panic!("预期 Protocol 错误，得到 {other:?}"),
    }
    server.await.unwrap();
}

// ---------- REQ-003: session/search + session/control + approval bypass ----------

#[tokio::test]
async fn search_unary_posts_query_and_parses_session_level_hits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        assert_eq!(body["method"], "session/search");
        assert_eq!(body["payload"]["args"]["query"], "deploy");
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {
                    "items": [{"sessionId": "sess-9", "snippet": "deploy 排查 …"}],
                    "hasMore": true
                }},
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let result = session::search(
        &http,
        &format!("http://{addr}"),
        "deploy",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].session_id, SessionId("sess-9".into()));
    assert_eq!(result.items[0].snippet, "deploy 排查 …");
    assert!(result.has_more);
    server.await.unwrap();
}

#[tokio::test]
async fn search_unary_surfaces_remote_error_code_not_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": false, "error": {"code": "PERMISSION_DENIED", "message": "无权限"}},
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let err = session::search(
        &http,
        &format!("http://{addr}"),
        "q",
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "PERMISSION_DENIED");
    assert_eq!(err.class(), ErrorClass::PermissionDenied);
    server.await.unwrap();
}

#[test]
fn control_item_parses_baseline_replacement_and_unknown() {
    let baseline = session::parse_control_item(&json!({
        "queues": [{"placement": "queued"}],
        "jobs": {},
        "projections": {"running": true}
    }))
    .expect("baseline parsed");
    match baseline {
        ControlItem::Baseline { projections, .. } => {
            assert_eq!(projections["running"], true);
        }
        other => panic!("预期 Baseline，得到 {other:?}"),
    }

    let queue = session::parse_control_item(&json!({"queue": {"placement": "steering"}}))
        .expect("queue replacement parsed");
    assert!(matches!(queue, ControlItem::Queue { .. }));
    let jobs = session::parse_control_item(&json!({"jobs": {"id": "j1"}})).unwrap();
    assert!(matches!(jobs, ControlItem::Jobs { .. }));
    let proj = session::parse_control_item(&json!({"projection": {"running": false}})).unwrap();
    assert!(matches!(proj, ControlItem::Projection { .. }));

    let unknown = session::parse_control_item(&json!({"type": "future-frame", "x": 1}))
        .expect("unknown preserved");
    match unknown {
        ControlItem::Unknown { kind, raw } => {
            assert_eq!(kind, "future-frame");
            assert_eq!(raw["x"], 1);
        }
        other => panic!("预期 Unknown，得到 {other:?}"),
    }
}

#[tokio::test]
async fn mux_pushes_streamless_frames_to_bypass_and_routes_streams() {
    // AC-003-07 前置：waterfall approval 帧（无 streamId）必须经旁路被订阅者
    // 收到；同时既有带 streamId 的 item 路由不受影响。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel::<Value>();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let first = ws.next().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
        opened_tx.send(frame.clone()).unwrap();
        let stream_id = frame["streamId"].as_u64().unwrap();

        ws.send(Message::Text(
            json!({
                "type": "approval/request",
                "clientId": "c-1",
                "eventId": "e-1",
                "signal": "waterfall"
            })
            .to_string(),
        ))
        .await
        .unwrap();
        ws.send(Message::Text(
            json!({"type": "item", "streamId": stream_id, "value": {"answer": 42}}).to_string(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    });

    let mux = Mux::connect(&format!("ws://{addr}"), "dsh-auth-0=v1.sig")
        .await
        .unwrap();
    let mut push = mux.subscribe_push();
    let mut stream = mux.open_stream("session/control", json!({})).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(1), opened_rx)
        .await
        .unwrap()
        .unwrap();

    // 推帧旁路先收到无 streamId 的完整原始帧。
    let pushed = tokio::time::timeout(Duration::from_secs(1), push.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pushed["type"], "approval/request");
    assert_eq!(pushed["clientId"], "c-1");
    assert_eq!(pushed["eventId"], "e-1");

    // 带 streamId 的 item 仍走既有流路由。
    let item = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(item, json!({"answer": 42}));
    server.await.unwrap();
}

#[tokio::test]
async fn approval_reply_posts_identity_and_outcome_vocabulary() {
    use dshtui::api::approval::{self, OUTCOME_METHOD};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        assert_eq!(body["method"], OUTCOME_METHOD);
        assert_eq!(body["payload"]["args"]["clientId"], "c-1");
        assert_eq!(body["payload"]["args"]["eventId"], "e-1");
        assert_eq!(body["payload"]["args"]["outcome"]["value"], "allowed-once");
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {"accepted": true}},
            }),
        )
        .await;
    });

    let event = dshtui::api::approval::parse_event(&json!({
        "type": "approval/request",
        "clientId": "c-1",
        "eventId": "e-1"
    }))
    .unwrap();
    let http = reqwest::Client::new();
    approval::reply(
        &http,
        &format!("http://{addr}"),
        &event,
        dshtui::api::types::ApprovalOutcome::AllowedOnce,
        Duration::from_secs(5),
    )
    .await
    .expect("outcome reply accepted");
    server.await.unwrap();
}

/// 回复一个 unary HTTP 响应（echo 请求的 rpcId；Content-Length 必须准确）。
async fn write_http_json(socket: &mut tokio::net::TcpStream, status: &str, body: &Value) {
    let body = body.to_string();
    socket
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn attachment_fetch_sends_envelope_and_decodes_base64_data() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, body) = read_http_request(&mut socket).await;
        assert!(
            headers.starts_with("POST /api/session/attachment HTTP/1.1"),
            "{headers}"
        );

        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["type"], "client-request");
        assert_eq!(request["method"], "session/attachment");
        assert_eq!(request["payload"]["args"]["sessionId"], "sess-1");
        assert_eq!(request["payload"]["args"]["attachmentId"], "att-9");
        let rpc_id = request["rpcId"].as_str().unwrap();

        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        // 用 base64 crate 独立编码（不依赖被测模块的编解码路径）。
        let data = base64::engine::general_purpose::STANDARD.encode(&png);
        write_http_json(
            &mut socket,
            "200 OK",
            &json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {
                    "ok": true,
                    "value": {
                        "attachment": {
                            "attachmentId": "att-9",
                            "mediaType": "image/png",
                            "bytes": 8,
                            "width": 640,
                            "height": 480,
                            "name": "design.png",
                            "originalDimensions": {"width": 1280, "height": 960}
                        },
                        "data": data
                    }
                }
            }),
        )
        .await;
    });

    let http = reqwest::Client::builder().build().unwrap();
    let base = format!("http://{addr}");
    let got = attachment::fetch(
        &http,
        &base,
        &SessionId("sess-1".into()),
        &AttachmentId("att-9".into()),
    )
    .await
    .unwrap();
    assert_eq!(got.attachment_id.0, "att-9");
    assert_eq!(got.media_type.0, "image/png");
    assert_eq!(got.bytes, 8);
    assert_eq!(got.width, 640);
    assert_eq!(got.height, 480);
    assert_eq!(got.name.as_deref(), Some("design.png"));
    let od = got.original_dimensions.as_ref().unwrap();
    assert_eq!((od.width, od.height), (1280, 960));
    assert_eq!(got.image_bytes, vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4]);
    server.await.unwrap();
}

#[tokio::test]
async fn attachment_fetch_error_envelope_classifies_by_code() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (_, body) = read_http_request(&mut socket).await;
        let request: Value = serde_json::from_slice(&body).unwrap();
        let rpc_id = request["rpcId"].as_str().unwrap();
        write_http_json(
            &mut socket,
            "200 OK",
            &json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {
                    "ok": false,
                    "error": {"code": "PERMISSION_DENIED", "message": "需审批"}
                }
            }),
        )
        .await;
    });

    let http = reqwest::Client::builder().build().unwrap();
    let err = attachment::fetch(
        &http,
        &format!("http://{addr}"),
        &SessionId("sess-1".into()),
        &AttachmentId("att-9".into()),
    )
    .await
    .unwrap_err();
    match &err {
        dshtui::api::ClientError::Remote { code, class, .. } => {
            assert_eq!(code, "PERMISSION_DENIED");
            assert_eq!(*class, ErrorClass::PermissionDenied, "权限类不自动重试");
        }
        other => panic!("expected Remote error, got {other:?}"),
    }
    assert_eq!(err.class(), ErrorClass::PermissionDenied);
    server.await.unwrap();
}

// ============================================================================
// REQ-005 Step 7：wire 边界事件 → TrajectoryWindow 投影对接（mock wire JSON
// 实解析后过 apply 漏斗；AC-005-06 边界行不丢的协议侧证据，D-23）。
// ============================================================================

#[test]
fn trajectory_wire_snapshot_boundary_events_survive_to_window_ac005_06() {
    // mock 一段官方形状 snapshot wire JSON（边界事件 + chunks + compaction +
    // request/header），经 serde 解析成 SessionHistoryRecord 后喂轨迹投影。
    let wire = r#"{
        "type": "snapshot",
        "cursor": 12,
        "hasMore": true,
        "records": [
            {"type": "event", "event": {"type": "request/header", "seq": 1, "time": 1, "data": {"reason": "initial"}}},
            {"type": "event", "event": {"type": "turn/start", "seq": 2, "time": 2, "data": {"turn": 1, "reason": "user-prompt"}}},
            {"type": "event", "event": {"type": "step/start", "seq": 3, "time": 3, "data": {"turn": 1, "step": 1, "reason": "max"}}},
            {"type": "event", "event": {"type": "user/message", "seq": 4, "time": 4, "data": {"content": "hi"}}},
            {"type": "event", "event": {"type": "assistant/message", "seq": 5, "time": 5, "data": {"turn": 1, "step": 1, "usage": {"input": 1}}}},
            {"type": "chunks", "event": {"type": "chunkrow/text-chunks", "texts": ["packed"], "turn": 1, "step": 1, "index": 0, "dt": []}},
            {"type": "event", "event": {"type": "tool/call", "seq": 6, "time": 6, "data": {"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{\"a\":1}"}}},
            {"type": "event", "event": {"type": "tool/result", "seq": 7, "time": 7, "data": {"turn": 1, "step": 1, "callId": "c1", "message": "ok"}}},
            {"type": "event", "event": {"type": "step/end", "seq": 8, "time": 8, "data": {"turn": 1, "step": 1}}},
            {"type": "event", "event": {"type": "turn/end", "seq": 9, "time": 9, "data": {"turn": 1, "reason": "stop"}}},
            {"type": "event", "event": {"type": "compaction/summary", "seq": 10, "time": 10, "data": {"summary": "pruned"}}}
        ]
    }"#;
    let frame: dshtui::api::types::FollowFrame = serde_json::from_str(wire).unwrap();
    let (records, has_more) = match frame {
        dshtui::api::types::FollowFrame::Snapshot {
            records, has_more, ..
        } => (records, has_more.unwrap_or(false)),
        _ => panic!("expect snapshot"),
    };
    // api 层解析出 10 event + 1 chunk = 11 records（边界不丢）。
    assert_eq!(records.len(), 11, "wire records 全保留");
    // 过轨迹投影漏斗（AppState 双写同 seam）。
    let mut w = dshtui::model::TrajectoryWindow::new(200);
    w.apply(dshtui::model::TrajIncoming::Snapshot {
        cursor: None,
        records,
        has_more,
        projections: None,
    });
    use dshtui::model::TrajKind;
    let kinds: Vec<TrajKind> = w.raw_rows().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            TrajKind::RequestHeader,
            TrajKind::TurnStart,
            TrajKind::StepStart,
            TrajKind::UserMessage,
            TrajKind::AssistantMessage,
            TrajKind::ToolCall,
            TrajKind::ToolResult,
            TrajKind::StepEnd,
            TrajKind::TurnEnd,
            TrajKind::Compaction,
        ],
        "wire 边界事件全链投影不丢（packed chunks 汇总进 assistant）"
    );
    // 无逐 delta 展开：10 event 行（chunks 不占独立行）。
    assert_eq!(w.len(), 10);
}

// ============================================================================
// REQ-006 Step 1：模型/命令/workspace/session 变更端点 wire mock（0.1.2-rc.1
// 实读：单 request 形参端点嵌套 {"request":{...}}；commands 平铺；AC-006 前序
// 契约）。
// ============================================================================

#[tokio::test]
async fn model_catalog_zero_arg_and_typed_catalog_response_ac006_01() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/session/modelCatalog HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["method"], "session/modelCatalog");
        // 零参数：args 为空对象（无 request 嵌套）。
        assert_eq!(body["payload"]["args"], json!({}));
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {
                    "default": {"provider": "deepseek_official", "model": "deepseek-chat"},
                    "routableProviders": ["deepseek_official"],
                    "groups": [
                        {"id": "deepseek_official", "name": "DeepSeek 官方", "models": [
                            {"id": "deepseek-chat", "name": "DeepSeek Chat",
                             "reasoning": {"efforts": [{"id": "low", "name": "Low"}], "defaultEffort": "low"}}
                        ]}
                    ],
                    "failures": []
                }}
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let catalog = dshtui::api::session::model_catalog(&http, &format!("http://{addr}"))
        .await
        .unwrap();
    assert_eq!(catalog.default.as_ref().unwrap().model, "deepseek-chat");
    assert_eq!(catalog.groups.len(), 1);
    assert_eq!(catalog.groups[0].models[0].id, "deepseek-chat");
    let reasoning = catalog.groups[0].models[0].reasoning.as_ref().unwrap();
    assert_eq!(reasoning.efforts[0].id, "low");
    assert_eq!(reasoning.default_effort.as_deref(), Some("low"));
    assert!(catalog.failures.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn select_model_nests_request_and_parses_selected_ac006_08() {
    use dshtui::api::types::SessionId;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/session/selectModel HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["method"], "session/selectModel");
        // 单 request 形参端点：业务字段嵌套在 args.request（0.1.2-rc.1 实读）。
        let req = &body["payload"]["args"]["request"];
        assert_eq!(req["sessionId"], "sess-1");
        assert_eq!(req["provider"], "deepseek_official");
        assert_eq!(req["model"], "deepseek-chat");
        assert_eq!(req["reasoningEffort"], "low");
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {"selected": {
                    "provider": "deepseek_official",
                    "model": "deepseek-chat",
                    "reasoningEffort": "low"
                }}}
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let sel = dshtui::api::session::select_model(
        &http,
        &format!("http://{addr}"),
        &SessionId("sess-1".into()),
        "deepseek_official",
        "deepseek-chat",
        Some("low"),
    )
    .await
    .unwrap();
    assert_eq!(sel.model, "deepseek-chat");
    assert_eq!(sel.reasoning_effort.as_deref(), Some("low"));
    server.await.unwrap();
}

#[tokio::test]
async fn session_fork_rename_create_nest_request_and_parse_typed_values() {
    use dshtui::api::types::SessionId;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let methods: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let methods2 = methods.clone();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_string();
            methods2
                .lock()
                .unwrap()
                .push((method.clone(), body["payload"]["args"].clone()));
            let rpc_id = body["rpcId"].as_str().unwrap().to_string();
            let value = match method.as_str() {
                "session/fork" => json!({"sessionId": "new-fork-1"}),
                "session/rename" => json!({"title": "新标题", "seq": 42}),
                _ => json!({"sessionId": "created-1", "agentPreset": "default"}),
            };
            write_json_response(
                &mut socket,
                json!({
                    "type": "server-response",
                    "rpcId": rpc_id,
                    "result": {"ok": true, "value": value}
                }),
            )
            .await;
        }
    });

    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    let fork = dshtui::api::session::fork(&http, &base, &SessionId("sess-1".into()), Some(7))
        .await
        .unwrap();
    assert_eq!(fork.session_id, "new-fork-1");
    let rename = dshtui::api::session::rename(&http, &base, &SessionId("sess-1".into()), "新标题")
        .await
        .unwrap();
    assert_eq!(rename.title, "新标题");
    assert_eq!(rename.seq, 42);
    let created = dshtui::api::session::create(&http, &base, Some("ws-1"), None)
        .await
        .unwrap();
    assert_eq!(created.session_id, "created-1");
    server.await.unwrap();

    let seen = methods.lock().unwrap();
    assert_eq!(seen[0].0, "session/fork");
    assert_eq!(seen[0].1["request"]["sessionId"], "sess-1");
    assert_eq!(seen[0].1["request"]["atSeq"], 7);
    assert_eq!(seen[1].0, "session/rename");
    assert_eq!(seen[1].1["request"]["title"], "新标题");
    assert_eq!(seen[2].0, "session/create");
    assert_eq!(seen[2].1["request"]["workspaceId"], "ws-1");
}

#[tokio::test]
async fn workspace_mutations_nest_request_and_archive_is_workspace_namespace() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert!(request.starts_with("POST /api/workspace/"));
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_string();
            let rpc_id = body["rpcId"].as_str().unwrap().to_string();
            match method.as_str() {
                "workspace/archiveSession" => {
                    // archiveSession 属 workspace namespace，请求仅 sessionId
                    // （FR-006-02 fact 修正）。
                    assert_eq!(body["payload"]["args"]["request"]["sessionId"], "sess-9");
                    assert!(body["payload"]["args"]["request"]
                        .get("workspaceId")
                        .is_none());
                    write_json_response(
                        &mut socket,
                        json!({"type": "server-response", "rpcId": rpc_id,
                               "result": {"ok": true, "value": {"archivedSessionIds": ["sess-9"]}}}),
                    )
                    .await;
                }
                _ => {
                    assert_eq!(body["payload"]["args"]["request"]["workspaceId"], "ws-2");
                    assert_eq!(body["payload"]["args"]["request"]["title"], "项目 B");
                    write_json_response(
                        &mut socket,
                        json!({"type": "server-response", "rpcId": rpc_id,
                        "result": {"ok": true, "value": {"workspace": {
                            "workspaceId": "ws-2", "path": "/p", "title": "项目 B",
                            "sessionIds": [], "createdAt": "x", "updatedAt": "y"
                        }}}}),
                    )
                    .await;
                }
            }
        }
    });

    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    let archive = dshtui::api::workspace::archive_session(&http, &base, "sess-9")
        .await
        .unwrap();
    assert_eq!(archive["archivedSessionIds"][0], "sess-9");
    let renamed = dshtui::api::workspace::rename_workspace(&http, &base, "ws-2", "项目 B")
        .await
        .unwrap();
    assert_eq!(renamed["workspace"]["title"], "项目 B");
    server.await.unwrap();
}

#[tokio::test]
async fn commands_flat_args_list_and_execute_undefined_tolerance_ac006_04() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let methods: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let methods2 = methods.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_string();
            methods2
                .lock()
                .unwrap()
                .push((method.clone(), body["payload"]["args"].clone()));
            let rpc_id = body["rpcId"].as_str().unwrap().to_string();
            let value = if method == "commands/list" {
                json!([
                    {"name": "plan", "description": "Plan mode", "input": {"hint": "off"}},
                    {"name": "help", "description": "Help"}
                ])
            } else {
                // execute 可能返回 undefined（服务器无输出）→ 容忍为成功空值。
                Value::Null
            };
            write_json_response(
                &mut socket,
                json!({"type": "server-response", "rpcId": rpc_id,
                       "result": {"ok": true, "value": value}}),
            )
            .await;
        }
    });

    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    let cmds = dshtui::api::commands::list(&http, &base, "agent-1")
        .await
        .unwrap();
    assert_eq!(cmds.len(), 2);
    assert_eq!(cmds[0].name, "plan");
    assert_eq!(cmds[0].input.as_ref().unwrap().hint, "off");
    assert_eq!(cmds[1].name, "help");
    let exec = dshtui::api::commands::execute(&http, &base, "agent-1", "/help", &[])
        .await
        .unwrap();
    assert!(exec.is_none(), "undefined 执行值被容忍为空成功");
    server.await.unwrap();

    let seen = methods.lock().unwrap();
    // commands 端点平铺 args（agentId 为 lookup scope，非嵌套）。
    assert_eq!(seen[0].0, "commands/list");
    assert_eq!(seen[0].1["agentId"], "agent-1");
    assert_eq!(seen[1].0, "commands/execute");
    assert_eq!(seen[1].1["agentId"], "agent-1");
    assert_eq!(seen[1].1["line"], "/help");
    assert_eq!(seen[1].1["images"], json!([]));
}

#[tokio::test]
async fn command_execute_surfaces_remote_error_code_ac006_13() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": false, "error": {
                    "code": "commands/not-found", "message": "未知命令"
                }}
            }),
        )
        .await;
    });

    let http = reqwest::Client::new();
    let err =
        dshtui::api::commands::execute(&http, &format!("http://{addr}"), "agent-1", "/nope", &[])
            .await
            .unwrap_err();
    match err {
        dshtui::api::ClientError::Remote { code, class, .. } => {
            assert_eq!(code, "commands/not-found");
            assert_eq!(class, ErrorClass::UserFacing);
        }
        other => panic!("expected Remote error, got {other:?}"),
    }
    server.await.unwrap();
}

// ============================================================================
// REQ-007 V0.4: subagents/goals/settings/skills/references/feedback/export wire
// mock（0.1.2-rc.1 实读：subagents interruptByParent 三个位置参数平铺；
// goals agentId+ref CAS；settings expectedRevision；export 同源 HTTP 路由）。
// ============================================================================

#[tokio::test]
async fn subagents_list_flat_args_and_typed_catalog() {
    use dshtui::api::types::{SubagentCatalog, SubagentListEntry};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/subagents/list HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["method"], "subagents/list");
        // parentSessionId 平铺（非 request 嵌套）。
        assert_eq!(body["payload"]["args"]["parentSessionId"], "parent-1");
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response", "rpcId": rpc_id,
                "result": {"ok": true, "value": {
                    "entries": [
                        {"kind": "child", "id": "c1", "activity": "running",
                         "hasChildren": true, "mode": "continuable", "label": "走查"},
                        {"kind": "diagnostic", "id": "c2", "reason": "corrupt"}
                    ],
                    "parentAvailable": true
                }}
            }),
        )
        .await;
    });
    let http = reqwest::Client::new();
    let cat: SubagentCatalog =
        dshtui::api::subagents::list(&http, &format!("http://{addr}"), "parent-1")
            .await
            .unwrap();
    assert_eq!(cat.entries.len(), 2);
    assert!(cat.parent_available);
    match &cat.entries[0] {
        SubagentListEntry::Child { id, activity, mode, .. } => {
            assert_eq!(id, "c1");
            assert_eq!(activity, "running");
            assert_eq!(mode.as_deref(), Some("continuable"));
        }
        other => panic!("expected child, got {other:?}"),
    }
    server.await.unwrap();
}

#[tokio::test]
async fn subagents_prompt_nests_request_and_returns_message_id() {
    use dshtui::api::types::{PromptContentPart, SubagentPromptRequest};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/subagents/prompt HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["method"], "subagents/prompt");
        let req = &body["payload"]["args"]["request"];
        assert_eq!(req["parentSessionId"], "parent-1");
        assert_eq!(req["childSessionId"], "c1");
        assert_eq!(req["mode"], "continuable");
        assert_eq!(req["content"][0]["type"], "text");
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response", "rpcId": rpc_id,
                "result": {"ok": true, "value": {"messageId": "msg-1", "accepted": true}}
            }),
        )
        .await;
    });
    let http = reqwest::Client::new();
    let receipt = dshtui::api::subagents::prompt(
        &http,
        &format!("http://{addr}"),
        &SubagentPromptRequest {
            request_id: "r-1".into(),
            parent_session_id: "parent-1".into(),
            child_session_id: "c1".into(),
            mode: "continuable".into(),
            content: vec![PromptContentPart::Text { text: "继续".into() }],
        },
    )
    .await
    .unwrap();
    assert_eq!(receipt.message_id.as_deref(), Some("msg-1"));
    assert_eq!(receipt.accepted, Some(true));
    server.await.unwrap();
}

#[tokio::test]
async fn subagents_interrupt_by_parent_three_flat_positional_args() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/subagents/interruptByParent HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["method"], "subagents/interruptByParent");
        // 三个位置参数平铺（wire 校正：非 request 嵌套）。
        let args = &body["payload"]["args"];
        assert_eq!(args["childSessionId"], "c1");
        assert_eq!(args["parentSessionId"], "parent-1");
        assert_eq!(args["mode"], "continuable");
        assert!(args.get("request").is_none(), "不得嵌套 request");
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response", "rpcId": rpc_id,
                "result": {"ok": true, "value": {"accepted": true}}
            }),
        )
        .await;
    });
    let http = reqwest::Client::new();
    let receipt = dshtui::api::subagents::interrupt_by_parent(
        &http,
        &format!("http://{addr}"),
        "c1",
        "parent-1",
    )
    .await
    .unwrap();
    assert_eq!(receipt.accepted, Some(true));
    server.await.unwrap();
}

#[tokio::test]
async fn goals_create_pause_clear_agent_id_and_cas_args() {
    use dshtui::api::types::{CreateGoalRequest, GoalRef};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_string();
            seen2
                .lock()
                .unwrap()
                .push((method.clone(), body["payload"]["args"].clone()));
            let rpc_id = body["rpcId"].as_str().unwrap().to_string();
            let value = match method.as_str() {
                "goals/create" => json!({"ref": {"id": "g1", "revision": 1}}),
                "goals/pause" => json!({"goal": {"id": "g1", "revision": 2,
                    "objective": "交付", "phase": "paused"}}),
                _ => json!({"id": "g1", "revision": 3}),
            };
            write_json_response(
                &mut socket,
                json!({
                    "type": "server-response", "rpcId": rpc_id,
                    "result": {"ok": true, "value": value}
                }),
            )
            .await;
        }
    });
    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    let created = dshtui::api::goals::create(
        &http,
        &base,
        "agent-1",
        &CreateGoalRequest { objective: "交付".into(), max_goal_rounds: None },
    )
    .await
    .unwrap();
    assert_eq!(created.id, "g1");
    assert_eq!(created.revision, 1);
    let paused = dshtui::api::goals::pause(
        &http,
        &base,
        "agent-1",
        &GoalRef { id: "g1".into(), revision: 1 },
    )
    .await
    .unwrap();
    assert_eq!(paused.phase, Some(dshtui::api::types::GoalPhase::Paused));
    let cleared = dshtui::api::goals::clear(
        &http,
        &base,
        "agent-1",
        &GoalRef { id: "g1".into(), revision: 2 },
    )
    .await
    .unwrap();
    assert_eq!(cleared.id, "g1");
    server.await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "goals/create");
    assert_eq!(seen[0].1["agentId"], "agent-1");
    assert_eq!(seen[0].1["request"]["objective"], "交付");
    assert_eq!(seen[1].0, "goals/pause");
    assert_eq!(seen[1].1["agentId"], "agent-1");
    assert_eq!(seen[1].1["ref"]["id"], "g1");
    assert_eq!(seen[1].1["ref"]["revision"], 1, "CAS revision 随请求上送");
    assert_eq!(seen[2].0, "goals/clear");
    assert_eq!(seen[2].1["ref"]["revision"], 2);
}

#[tokio::test]
async fn settings_describe_and_update_with_expected_revision_cas() {
    use dshtui::api::types::{SettingsNamespaceView, SettingsPathOpView};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_string();
            let rpc_id = body["rpcId"].as_str().unwrap().to_string();
            match method.as_str() {
                "settings/describe" => {
                    assert_eq!(body["payload"]["args"], json!({}));
                    write_json_response(
                        &mut socket,
                        json!({
                            "type": "server-response", "rpcId": rpc_id,
                            "result": {"ok": true, "value": {
                                "writable": true, "hasDocument": true,
                                "namespaces": [{
                                    "ns": "ui-theme", "schema": {"type": "object"},
                                    "value": {"preference": "dark"},
                                    "applies": "live", "secrets": [], "revision": 4
                                }]
                            }}
                        }),
                    )
                    .await;
                }
                _ => {
                    let args = &body["payload"]["args"];
                    assert_eq!(args["ns"], "locale");
                    assert_eq!(args["expectedRevision"], 4);
                    assert_eq!(args["patch"]["preference"], "zh-CN");
                    write_json_response(
                        &mut socket,
                        json!({
                            "type": "server-response", "rpcId": rpc_id,
                            "result": {"ok": true, "value": {
                                "namespace": {
                                    "ns": "locale", "schema": {"type": "object"},
                                    "value": {"preference": "zh-CN"},
                                    "applies": "live", "secrets": [], "revision": 5
                                }
                            }}
                        }),
                    )
                    .await;
                }
            }
        }
    });
    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    let describe = dshtui::api::settings::describe(&http, &base).await.unwrap();
    assert!(describe.writable);
    assert_eq!(describe.namespaces.len(), 1);
    assert_eq!(describe.namespaces[0].ns, "ui-theme");
    assert_eq!(describe.namespaces[0].revision, 4);
    let updated: SettingsNamespaceView = dshtui::api::settings::update(
        &http,
        &base,
        "locale",
        json!({"preference": "zh-CN"}),
        Some(4),
    )
    .await
    .unwrap();
    assert_eq!(updated.ns, "locale");
    assert_eq!(updated.revision, 5);
    // mutate 路径 op 形状（单元覆盖于模块内测试，此处仅确认编译可达）。
    let _op = SettingsPathOpView {
        path: "preference".into(),
        op_kind: "set".into(),
        value: Some(json!("dark")),
    };
    server.await.unwrap();
}

#[tokio::test]
async fn skills_list_session_scoped_and_typed_entries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("POST /api/skills/list HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["method"], "skills/list");
        assert_eq!(body["payload"]["args"]["sessionId"], "sess-1");
        let rpc_id = body["rpcId"].as_str().unwrap().to_string();
        write_json_response(
            &mut socket,
            json!({
                "type": "server-response", "rpcId": rpc_id,
                "result": {"ok": true, "value": {
                    "skills": [
                        {"name": "bash", "description": "执行 shell", "modelInvocable": true}
                    ]
                }}
            }),
        )
        .await;
    });
    let http = reqwest::Client::new();
    let skills = dshtui::api::skills::list(&http, &format!("http://{addr}"), "sess-1")
        .await
        .unwrap();
    assert_eq!(skills.skills.len(), 1);
    assert_eq!(skills.skills[0].name, "bash");
    server.await.unwrap();
}

#[tokio::test]
async fn references_two_sources_flat_agent_args() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let methods: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let methods2 = methods.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_string();
            methods2
                .lock()
                .unwrap()
                .push((method.clone(), body["payload"]["args"].clone()));
            let rpc_id = body["rpcId"].as_str().unwrap().to_string();
            let value = if method == "fileReferences/list" {
                json!([{"path": "src/api/mod.rs", "kind": "file"},
                       {"path": "src/api", "kind": "directory"}])
            } else {
                json!([{"sessionId": "s1", "label": "部署排查", "mention": "@[部署排查](dsh-session:s1)",
                        "sameWorkspace": true, "createdAt": 1}])
            };
            write_json_response(
                &mut socket,
                json!({
                    "type": "server-response", "rpcId": rpc_id,
                    "result": {"ok": true, "value": value}
                }),
            )
            .await;
        }
    });
    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    let files = dshtui::api::references::file_references(&http, &base, "agent-1", "src/api")
        .await
        .unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].kind, "file");
    let sessions = dshtui::api::references::session_candidates(&http, &base, "agent-1", "部署")
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "s1");
    server.await.unwrap();
    let seen = methods.lock().unwrap();
    assert_eq!(seen[0].0, "fileReferences/list");
    assert_eq!(seen[0].1["agentId"], "agent-1");
    assert_eq!(seen[0].1["query"], "src/api");
    assert_eq!(seen[1].0, "sessionReferenceResolver/candidates");
    assert_eq!(seen[1].1["agentId"], "agent-1");
}

#[tokio::test]
async fn feedback_put_nests_cas_fields_and_lists() {
    use dshtui::api::types::MessageFeedbackPutRequest;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let method = body["method"].as_str().unwrap().to_string();
            let rpc_id = body["rpcId"].as_str().unwrap().to_string();
            match method.as_str() {
                "messageFeedback/put" => {
                    let req = &body["payload"]["args"]["request"];
                    assert_eq!(req["sessionId"], "sess-1");
                    assert_eq!(req["messageId"], "m1");
                    assert_eq!(req["rating"], "positive");
                    assert_eq!(req["ifVersion"], 2);
                    write_json_response(
                        &mut socket,
                        json!({"type": "server-response", "rpcId": rpc_id,
                               "result": {"ok": true, "value": {"accepted": true}}}),
                    )
                    .await;
                }
                _ => {
                    assert_eq!(body["payload"]["args"]["sessionId"], "sess-1");
                    write_json_response(
                        &mut socket,
                        json!({"type": "server-response", "rpcId": rpc_id,
                               "result": {"ok": true, "value": {
                                   "items": [{"messageId": "m1", "rating": "positive"}]
                               }}}),
                    )
                    .await;
                }
            }
        }
    });
    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    let put = dshtui::api::feedback::put(
        &http,
        &base,
        &MessageFeedbackPutRequest {
            session_id: "sess-1".into(),
            message_id: "m1".into(),
            rating: "positive".into(),
            note: None,
            if_version: Some(2),
        },
    )
    .await
    .unwrap();
    assert_eq!(put["accepted"], true);
    let list = dshtui::api::feedback::list(&http, &base, "sess-1").await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].rating, "positive");
    server.await.unwrap();
}

#[tokio::test]
async fn export_downloads_official_route_and_streams_to_file() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // GET 请求无 body；读头到 \r\n\r\n。
        let mut raw = Vec::new();
        let mut buf = [0_u8; 2048];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&raw).into_owned();
        assert!(
            head.starts_with("GET /api/session.export?sessionId=sess-1&includeDescendants=true HTTP/1.1"),
            "head={head}"
        );
        // 模拟流式 ZIP 响应体（两段，验证逐 chunk 落盘）。
        let body = b"PK\x03\x04export-bytes";
        let len = body.len();
        socket
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {len}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(body).await.unwrap();
        socket.flush().await.unwrap();
    });
    let http = reqwest::Client::new();
    // 测试自建独立目录（进程内 /tmp 同一会话可见；结束清理）。
    let tmp = std::env::temp_dir().join(format!("dshtui-export-test-{}", std::process::id()));
    let target = tmp.join("session-export.zip");
    let receipt = dshtui::api::export::download_export(
        &http,
        &format!("http://{addr}"),
        "sess-1",
        &target,
    )
    .await
    .unwrap();
    assert_eq!(receipt.bytes, body_len());
    assert!(target.exists(), "目标文件落盘");
    let tmp_name = format!(
        "session-export.zip.tmp-{}",
        std::process::id()
    );
    assert!(
        !tmp.join(tmp_name).exists(),
        "临时文件已改名不残留"
    );
    let data = std::fs::read(&target).unwrap();
    assert_eq!(&data, b"PK\x03\x04export-bytes");
    // 清理临时目录（测试自建，会话内清理）。
    let _ = std::fs::remove_dir_all(&tmp);
    server.await.unwrap();
}

fn body_len() -> u64 {
    b"PK\x03\x04export-bytes".len() as u64
}

#[test]
fn control_jobs_parse_baseline_replacement_and_tolerates_bad_rows() {
    // replacement frame: bare array
    let jobs = dshtui::api::session::parse_jobs(&json!([
        {"id": "j1", "kind": "session/prompt", "label": "跑测试",
         "status": "running", "startedAt": 1},
        {"id": "j2", "kind": "tool/call", "label": "bash", "status": "completed"}
    ]));
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].id, "j1");
    assert_eq!(jobs[0].status, Some(dshtui::api::types::SessionJobStatus::Running));
    assert_eq!(jobs[1].status, Some(dshtui::api::types::SessionJobStatus::Completed));

    // baseline per-session object + wrapper
    let jobs = dshtui::api::session::parse_jobs(&json!({
        "sess-1": [{"id": "j1", "kind": "k", "label": "l", "status": "stopping"}]
    }));
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, Some(dshtui::api::types::SessionJobStatus::Stopping));
    let jobs = dshtui::api::session::parse_jobs(&json!({
        "items": [{"id": "j3", "kind": "k", "label": "l", "status": "failed"}]
    }));
    assert_eq!(jobs[0].status, Some(dshtui::api::types::SessionJobStatus::Failed));

    // bad row skipped, never fatal; empty tolerated
    let jobs = dshtui::api::session::parse_jobs(&json!([
        {"id": "ok", "kind": "k", "label": "l", "status": "killed"},
        {"id": 42}
    ]));
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, Some(dshtui::api::types::SessionJobStatus::Killed));
    assert!(dshtui::api::session::parse_jobs(&json!({})).is_empty());
    assert!(dshtui::api::session::parse_jobs(&json!(null)).is_empty());
}
