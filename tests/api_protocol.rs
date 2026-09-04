use std::time::Duration;

use dshtui::api::auth::authenticate;
use dshtui::api::envelope::{remote_error, ClientRequest, ErrorClass, RpcError, ServerResponse};
use dshtui::api::session::{self, cancel, prompt, AcceptedValue};
use dshtui::api::types::{
    ControlItem, FollowFrame, PromptContentPart, PromptMode, PromptRequest, SessionId,
    SessionRequestId, SessionSeq,
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
