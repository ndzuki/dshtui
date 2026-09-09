//! REQ-009 mock agent-server 协议测试（`tests/api_protocol.rs` 同款内联
//! `TcpListener` 模式）：/health、/agents（16 字段 + `x-agents-finished`）、
//! /kb-stats（嵌套 hist 桶边界）、/agent/chat（多轮 sessionId 复用 + 失败）。
//!
//! 覆盖 REQ-009 §8 验收口径：字段/响应头/嵌套 hist 桶边界断言 + chat
//! body/resp + errorCode + 错误分类（网络失败 retryable / 业务错误码）。

use dshtui::api::monitor::{ChatRequest, MonitorClient};
use dshtui::model::agent_roster::AgentRosterEntry;
use dshtui::model::kb_stats::{KbStatsSnapshot, KB_DURATION_BOUNDARIES};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 极简 HTTP/1.1 应答 helper（Connection: close，一轮一连接）。
async fn respond(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &str,
) {
    let mut resp = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\n");
    for (k, v) in headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    socket.write_all(resp.as_bytes()).await.unwrap();
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            // Content-Length 后的 body
            let body_start = head_end;
            let body = String::from_utf8_lossy(&buf[body_start..]).to_string();
            return (head, body);
        }
    }
    (String::new(), String::new())
}

const AGENTS_BODY: &str = r#"[
  {"sessionId":"session-abc","phase":"implementing","task":"修管线\n第二行","project":"release-manager",
   "taskId":"TASK-077","status":"working","taskStatus":"implementing","elapsed":3601,
   "lastEventAt":1788515073345,"seq":42,"label":"子代理A","kind":"subagent",
   "parentSessionId":"session-parent","delegationDepth":2,
   "provider":"deepseek_magic","model":"deepseek-v4-pro","unknownField":"x"},
  {"sessionId":"session-idle","phase":"","task":"闲聊","project":"","taskId":"","status":"idle",
   "taskStatus":"","elapsed":61,"lastEventAt":1788515073000,"seq":7,"label":"","kind":"session",
   "parentSessionId":"","delegationDepth":0,"provider":"","model":""}
]"#;

#[tokio::test]
async fn agents_endpoint_parses_full_entries_and_finished_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (head, _) = read_request(&mut socket).await;
        assert!(head.starts_with("GET /agents HTTP/1.1"), "head={head}");
        respond(
            &mut socket,
            "200 OK",
            &[("x-agents-finished", "7")],
            AGENTS_BODY,
        )
        .await;
    });

    let client = MonitorClient::new(&format!("http://{addr}")).unwrap();
    let resp = client.agents().await.unwrap();
    assert_eq!(resp.finished, 7, "x-agents-finished 响应头");
    assert_eq!(resp.entries.len(), 2);

    // wire → 内部模型：16 字段族逐项断言（AC-009-02/04）。
    let e = AgentRosterEntry::from_wire(&resp.entries[0]);
    assert_eq!(e.session_id.get(), "session-abc");
    assert_eq!(e.phase, "implementing");
    assert_eq!(e.task_first_line(), "修管线");
    assert_eq!(e.project, "release-manager");
    assert_eq!(e.task_id, "TASK-077");
    assert_eq!(e.status, dshtui::model::AgentStatus::Working);
    assert_eq!(e.task_status, "implementing");
    assert_eq!(e.elapsed_sec, 3601);
    assert_eq!(e.last_event_at_ms, 1788515073345);
    assert_eq!(e.seq, 42);
    assert_eq!(e.label, "子代理A");
    assert_eq!(e.kind, dshtui::model::AgentKind::Subagent);
    assert_eq!(e.parent_session_id.as_deref(), Some("session-parent"));
    assert_eq!(e.delegation_depth, 2);
    assert_eq!(e.provider, "deepseek_magic");
    assert_eq!(e.model, "deepseek-v4-pro");
    // 详情字段与 /agents 条目一致（AC-009-04 口径）。
    assert_eq!(e.display_name(), "子代理A");

    server.await.unwrap();
}

#[tokio::test]
async fn kb_stats_endpoint_nested_hist_and_boundaries() {
    let body = r#"{"totals":{"hits":10,"misses":2,"empty":1,"errs":0,"skipped":1,
      "searches":12,"avgMs":345,"hist":{"boundaries":[0,100,500,1000,2000,4000,16000],
      "counts":[3,2,1,4,1,1,0]}},
      "window":{"hits":1,"misses":0,"empty":0,"errs":0,"skipped":0,"searches":1,
      "avgMs":120,"hist":{"boundaries":[0,100,500,1000,2000,4000,16000],
      "counts":[0,0,0,0,1,0,0]}},
      "lastLogAt":1788515073345,"restored":true}"#;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (head, _) = read_request(&mut socket).await;
        assert!(head.starts_with("GET /kb-stats HTTP/1.1"));
        respond(&mut socket, "200 OK", &[], body).await;
    });

    let client = MonitorClient::new(&format!("http://{addr}")).unwrap();
    let wire = client.kb_stats().await.unwrap();
    let snap = KbStatsSnapshot::from_wire(&wire).unwrap();
    assert_eq!(snap.totals.hits, 10);
    assert_eq!(snap.window.hist.counts[4], 1, "window 段当前小时桶");
    assert!(
        snap.boundaries_match_agent_server(),
        "桶边界沿 KB_DURATION_BOUNDARIES（AC-009-07）"
    );
    assert_eq!(snap.totals.hist.boundaries, KB_DURATION_BOUNDARIES.to_vec());
    assert!(snap.restored);

    server.await.unwrap();
}

#[tokio::test]
async fn chat_endpoint_multi_turn_session_id_and_request_shape() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(4);
    let server = tokio::spawn(async move {
        // 第一轮：无 sessionId → 返回新会话。
        {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (_head, body) = read_request(&mut socket).await;
            let req: serde_json::Value = serde_json::from_str(&body).unwrap();
            seen_tx.send(req).await.unwrap();
            respond(
                &mut socket,
                "200 OK",
                &[],
                r#"{"text":"回答1","outcome":"completed","sessionId":"session-chat-1"}"#,
            )
            .await;
        }
        // 第二轮：带 sessionId（多轮复用）→ 校验透传。
        {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (_head, body) = read_request(&mut socket).await;
            let req: serde_json::Value = serde_json::from_str(&body).unwrap();
            seen_tx.send(req).await.unwrap();
            respond(
                &mut socket,
                "200 OK",
                &[],
                r#"{"text":"回答2","outcome":"completed","sessionId":"session-chat-1"}"#,
            )
            .await;
        }
    });

    let client = MonitorClient::new(&format!("http://{addr}")).unwrap();
    let first = client
        .chat(&ChatRequest {
            message: "问题1".into(),
            provider: "deepseek_magic".into(),
            model: "deepseek-v4-pro".into(),
            reasoning_effort: Some("medium".into()),
            session_id: None,
            kb_query: Some("任务标题".into()),
            project: Some("proj".into()),
        })
        .await
        .unwrap();
    assert_eq!(first.text, "回答1");
    assert_eq!(first.session_id, "session-chat-1");

    let second = client
        .chat(&ChatRequest {
            message: "问题2".into(),
            provider: "deepseek_magic".into(),
            model: "deepseek-v4-pro".into(),
            reasoning_effort: Some("medium".into()),
            session_id: Some("session-chat-1".into()),
            kb_query: None,
            project: None,
        })
        .await
        .unwrap();
    assert_eq!(second.text, "回答2");

    // 请求形状断言（首条带 kbQuery/project，多轮复用 sessionId，AC-009-06）。
    let req1 = seen_rx.recv().await.unwrap();
    assert_eq!(req1["message"], "问题1");
    assert!(req1.get("sessionId").is_none(), "首条无 sessionId");
    assert_eq!(req1["kbQuery"], "任务标题");
    assert_eq!(req1["project"], "proj");
    let req2 = seen_rx.recv().await.unwrap();
    assert_eq!(req2["sessionId"], "session-chat-1", "多轮复用 sessionId");
    assert!(req2.get("kbQuery").is_none(), "多轮不再带 kbQuery");

    server.await.unwrap();
}

#[tokio::test]
async fn chat_failure_scenarios_are_classified() {
    // 业务错误码：200 + errorCode → 可读业务错误（非 retryable）。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        respond(
            &mut socket,
            "200 OK",
            &[],
            r#"{"text":"","outcome":"error","sessionId":"s1","errorCode":"TIMEOUT","error":"模型超时"}"#,
        )
        .await;
        let (mut socket2, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket2).await;
        respond(
            &mut socket2,
            "500 Internal Server Error",
            &[],
            r#"{"error":"boom"}"#,
        )
        .await;
        let (mut socket3, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket3).await;
        // 非 JSON 200 → Json 分类错误。
        respond(&mut socket3, "200 OK", &[], "not-json").await;
    });

    let client = MonitorClient::new(&format!("http://{addr}")).unwrap();
    let req = || ChatRequest {
        message: "x".into(),
        provider: "p".into(),
        model: "m".into(),
        ..Default::default()
    };

    let r1 = client.chat(&req()).await.unwrap();
    assert_eq!(r1.error_code, "TIMEOUT");
    assert_eq!(r1.error, "模型超时");

    let r2 = client.chat(&req()).await.unwrap_err();
    assert!(r2.is_retryable(), "5xx 可重试: {r2}");
    assert!(r2.to_string().contains("500"), "状态码可读: {r2}");

    let r3 = client.chat(&req()).await.unwrap_err();
    assert!(!r3.is_retryable(), "JSON 解析失败不可重试");
    assert_eq!(r3.code(), "json");

    server.await.unwrap();
}

#[tokio::test]
async fn health_and_connection_refused_classification() {
    // /health 200 → ok。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (head, _) = read_request(&mut socket).await;
        assert!(head.starts_with("GET /health HTTP/1.1"));
        respond(&mut socket, "200 OK", &[], r#"{"ok":true}"#).await;
    });
    let client = MonitorClient::new(&format!("http://{addr}")).unwrap();
    client.health().await.unwrap();
    server.await.unwrap();

    // 连接拒绝 → Transport（retryable，AC-009-01 启动指引路径）。
    let refused = MonitorClient::new("http://127.0.0.1:1").unwrap();
    let err = refused.health().await.unwrap_err();
    assert!(err.is_retryable(), "连接拒绝可重试: {err}");
    assert_eq!(err.code(), "transport");
}
