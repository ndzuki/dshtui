use std::time::Duration;

use dshtui::api::auth::authenticate;
use dshtui::api::envelope::{remote_error, ClientRequest, ErrorClass, RpcError, ServerResponse};
use dshtui::api::types::{FollowFrame, SessionSeq};
use dshtui::api::Mux;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

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
