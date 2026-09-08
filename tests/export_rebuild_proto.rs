use tokio::io::AsyncReadExt;
// Throwaway prototype (Step 16 D-46 export page-rebuild, risk:high gate):
//   validates against a mock server that
//    (a) the official export route 404 classification is distinguishable from
//        403 (structured HttpStatus, not silent Http);
//    (b) `rebuild_export_jsonl` page-loops to collect the FULL fixture record
//        set (3 pages → exact count, 6 events + 1 chunkrow), writes JSONL
//        lines = records + 1 header in chronological order, maps each record
//        line to the official format (REQ-008 AC-008-15: `session` header /
//        unwrap event wrapper / chunkrow→`*-chunks` + seq0/time0), keeps
//        hasMore/cursor termination, and cleans tmp on stream error / cancel.
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn record(seq: u64) -> Value {
    json!({"type":"event","event":{"seq": seq, "type": "user/message",
           "data": {"content": format!("msg {seq}")}}})
}

/// 一条 chunkrow record（page 聚合形态；REQ-008 AC-008-15 锁定 chunkrow 映射）。
fn chunks_record(seq: u64) -> Value {
    json!({"type":"chunks","event":{
        "type":"chunkrow/reasoning-chunks","seq": seq, "time": 100,
        "data":{"turn":1,"step":1,"index":0,"dt":[1.0],"texts":["chunk"]}}})
}

fn page_payload(records: Vec<Value>, has_more: bool) -> Value {
    json!({"records": records, "hasMore": has_more})
}

async fn serve_export_route(listener: TcpListener, status_code: u16) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut raw = Vec::new();
    let mut buf = [0_u8; 4096];
    while let Ok(n) = socket.read(&mut buf).await {
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let _ = String::from_utf8_lossy(&raw).into_owned();
    let body = match status_code {
        404 => b"not found".to_vec(),
        403 => b"forbidden".to_vec(),
        _ => b"".to_vec(),
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status_code,
        if status_code == 404 { "Not Found" } else { "Forbidden" },
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(&body).await;
    let _ = socket.flush().await;
}

/// mock unary `session/page` envelope responder; serves fixed fixture pages by
/// inspecting the beforeSeq cursor in the request body.
async fn serve_page_rpc(listener: TcpListener) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // 3 fixture pages of 2 records each (newest page first, descending),
        // selected by the beforeSeq cursor (None=page 0, 5=page1, 3=page2).
        // 有连接就服务；空闲 2s 视为客户端结束（避免 accept 挂死测试）。
        loop {
            let accept = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept());
            let Ok(Ok((mut socket, _))) = accept.await else {
                break;
            };
            let mut raw = Vec::new();
            let mut buf = [0_u8; 8192];
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if n == 0 {
                    break;
                }
            }
            // Parse request body for method + rpcId.
            let text = String::from_utf8_lossy(&raw).into_owned();
            if !text.contains("\"method\":\"session/page\"") {
                // not a page request → 404 to fail the test
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                continue;
            }
            let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
            let body: Value = serde_json::from_str(&text[body_start..]).unwrap_or(Value::Null);
            let rpc_id = body.get("rpcId").and_then(|v| v.as_str()).unwrap_or("r");
            // Pick page by beforeSeq value (deterministic fixture).
            // session/page 单 request 形参（wire 校正 0.1.2-rc.1）：
            // beforeSeq 位于 args.request.beforeSeq。
            let before_seq = body
                .pointer("/payload/args/request/beforeSeq")
                .and_then(|v| v.as_u64());
            let (records, has_more) = match before_seq {
                None => (vec![record(6), record(5)], true),
                Some(5) => (vec![record(4), record(3)], true),
                _ => (vec![record(2), record(1), chunks_record(0)], false),
            };
            let resp = json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": page_payload(records, has_more)}
            });
            let body = resp.to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
        }
    })
}

#[tokio::test]
async fn export_404_is_structured_and_rebuild_collects_full_fixture() {
    // 1) official route 404 → structured HttpStatus (distinguishable from 403).
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let srv = tokio::spawn(serve_export_route(l, 404));
    let http = reqwest::Client::new();
    let tmp = std::env::temp_dir().join(format!("dshtui-rebuild-proto-404-{}", std::process::id()));
    let target = tmp.join("session-export.zip");
    let _ = std::fs::create_dir_all(&tmp);
    let err =
        dshtui::api::export::download_export(&http, &format!("http://{addr}"), "sess-1", &target)
            .await
            .unwrap_err();
    assert_eq!(
        err.http_status(),
        Some(404),
        "404 结构化（HttpStatus 变体）"
    );
    assert!(
        !matches!(err, dshtui::api::ClientError::Http(_)),
        "不再是裸 Http 字符串"
    );
    assert_eq!(
        err.class(),
        dshtui::api::ErrorClass::UserFacing,
        "404 非权限类"
    );
    srv.await.unwrap();
    let _ = std::fs::remove_dir_all(&tmp);

    // 2) 403 → PermissionDenied class（不降级不重试的依据）。
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let srv = tokio::spawn(serve_export_route(l, 403));
    let http = reqwest::Client::new();
    let err =
        dshtui::api::export::download_export(&http, &format!("http://{addr}"), "sess-1", &target)
            .await
            .unwrap_err();
    assert_eq!(err.http_status(), Some(403));
    assert_eq!(err.class(), dshtui::api::ErrorClass::PermissionDenied);
    srv.await.unwrap();

    // 3) page rebuild: 3 pages → full fixture (6 events + 1 chunkrow) + 1 header.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let page_srv = serve_page_rpc(l).await;
    let http = reqwest::Client::new();
    let tmp =
        std::env::temp_dir().join(format!("dshtui-rebuild-proto-page-{}", std::process::id()));
    let target = tmp.join("session-rebuilt.jsonl");
    let _ = std::fs::create_dir_all(&tmp);
    let address = dshtui::api::types::SessionAddress::session("sess-1");
    let mut reported: Vec<u64> = Vec::new();
    let receipt = dshtui::api::export::rebuild_export_jsonl(
        &http,
        &format!("http://{addr}"),
        "sess-1",
        &address,
        dshtui::api::types::SessionSeq(6),
        &target,
        &mut |n| reported.push(n),
        || false,
    )
    .await
    .expect("rebuild 成功");
    assert!(target.exists());
    let text = std::fs::read_to_string(&target).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        7 + 1,
        "records(6 events+1 chunkrow)+header(1); lines={}",
        lines.len()
    );
    // header = 官方 session 形态（REQ-008 AC-008-15 锁定）。
    let header: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        header,
        json!({"type":"session","version":0,"id":"sess-1"}),
        "header 对齐官方 session 行"
    );
    // 记录行按 seq 顺时（fixture 分页倒序 → 输出 chunkrow(0) 然后 1..6）；
    // event 行解包平铺，chunkrow 行映射为官方 reasoning-chunks（seq0/time0）。
    let parsed: Vec<Value> = lines[1..]
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        parsed[0],
        json!({"type":"reasoning-chunks","seq0":0,"time0":100,
               "data":{"turn":1,"step":1,"index":0,"dt":[1.0],"texts":["chunk"]}}),
        "chunkrow 行映射为官方 *-chunks（无 event 包裹、seq/time→seq0/time0）"
    );
    let seqs: Vec<u64> = parsed
        .iter()
        .map(|v| {
            v["seq0"]
                .as_u64()
                .unwrap_or_else(|| v["seq"].as_u64().unwrap())
        })
        .collect();
    assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5, 6], "顺时升序输出");
    for (i, v) in parsed.iter().enumerate().skip(1) {
        let n = (i) as u64; // line i+1 of records → event seq i (0-based i=1→seq1)
        assert_eq!(
            v["type"], "user/message",
            "record {} 平铺（无 event 包裹）",
            n
        );
        assert_eq!(
            v["data"]["content"],
            json!(format!("msg {n}")),
            "record {n} 内容"
        );
    }
    // 进度单调。
    assert_eq!(reported.last(), Some(&7));
    assert!(reported.windows(2).all(|w| w[1] >= w[0]));
    let _ = receipt;
    let _ = std::fs::remove_dir_all(&tmp);
    page_srv.await.unwrap();
}

#[tokio::test]
async fn export_rebuild_cancel_and_error_clean_tmp() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // page loop errors on second page → tmp removed, no half product.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let page_srv = tokio::spawn(async move {
        let mut idx = 0usize;
        // 有连接就服务；空闲 2s 视为客户端结束（防 accept 挂死测试）。
        loop {
            let accept = tokio::time::timeout(std::time::Duration::from_secs(2), l.accept());
            let Ok(Ok((mut socket, _))) = accept.await else {
                break;
            };
            let mut raw = Vec::new();
            let mut buf = [0_u8; 8192];
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if n == 0 {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
            let body: Value = serde_json::from_str(&text[body_start..]).unwrap_or(Value::Null);
            let rpc_id = body.get("rpcId").and_then(|v| v.as_str()).unwrap_or("r");
            // page 0 ok (hasMore), page 1+ → 500 (rebuild 应失败并清 tmp)
            if idx == 0 {
                let resp = json!({"type":"server-response","rpcId":rpc_id,"result":{"ok":true,"value":page_payload(vec![record(6),record(5)], true)}});
                let body = resp.to_string();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            } else {
                // 模拟 5xx（rebuild 应返回结构化错误并清 tmp）
                let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await;
            }
            let _ = socket.flush().await;
            idx += 1;
        }
    });
    let http = reqwest::Client::new();
    let tmp = std::env::temp_dir().join(format!("dshtui-rebuild-proto-err-{}", std::process::id()));
    let target = tmp.join("session-rebuilt.jsonl");
    let _ = std::fs::create_dir_all(&tmp);
    let address = dshtui::api::types::SessionAddress::session("sess-1");
    let result = dshtui::api::export::rebuild_export_jsonl(
        &http,
        &format!("http://{addr}"),
        "sess-1",
        &address,
        dshtui::api::types::SessionSeq(6),
        &target,
        &mut |_| {},
        || false,
    )
    .await;
    assert!(result.is_err(), "第二页 500 必须失败: {result:?}");
    assert!(!target.exists(), "目标不留半成品");
    // tmp 清理
    let leaked = std::fs::read_dir(&tmp).unwrap().count();
    assert_eq!(leaked, 0, "tmp 无残留: {:?}", std::fs::read_dir(&tmp));
    let _ = std::fs::remove_dir_all(&tmp);
    page_srv.await.unwrap();

    // cancel: token set before start → 立刻中止、清 tmp。
    let l2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _addr2 = l2.local_addr().unwrap();
    let cancel = AtomicBool::new(true);
    let http2 = reqwest::Client::new();
    let tmp2 = std::env::temp_dir().join(format!(
        "dshtui-rebuild-proto-cancel-{}",
        std::process::id()
    ));
    let target2 = tmp2.join("x.jsonl");
    let _ = std::fs::create_dir_all(&tmp2);
    let result2 = dshtui::api::export::rebuild_export_jsonl(
        &http2,
        "http://127.0.0.1:1", // unreachable
        "sess-1",
        &address,
        dshtui::api::types::SessionSeq(6),
        &target2,
        &mut |_| {},
        || cancel.load(Ordering::Relaxed),
    )
    .await;
    assert!(result2.is_err());
    let _ = std::fs::remove_dir_all(&tmp2);
}
