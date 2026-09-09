//! live_smoke.rs —— REQ-008 Step 6 / AC-008-14 live 契约冒烟（integration test）。
//!
//! 目的：把 Step 6 曾以一次性 curl 验证的 live 契约冒烟，落成可复现、可进
//! CI 的产物（`.github/workflows/ci.yml` 在 `tests/live_smoke.rs` 存在时才
//! 运行 live smoke）。
//!
//! ## env 门控（CI 默认跳过）
//! - 所有测试 `#[ignore]`：`cargo test --all-targets` 不碰 live，CI 以离线
//!   mock（api_protocol / export_rebuild_proto）为准；
//! - 仅 `DSH_TOKEN`（必填）设置时，`cargo test --test live_smoke -- --ignored`
//!   才会真正连真实 dsh web（CI 由 `DSHTUI_LIVE_SMOKE=1` + 仓库 secret 注入）；
//! - `DSHTUI_LIVE_BASE` 缺省 `http://127.0.0.1:3080`；
//! - `DSHTUI_LIVE_SESSION` 缺省 REQ-008 证据会话
//!   `session-25536e2c-f8b9-4bcf-a16b-0baa085fa362`。
//!
//! token 只用于连接，绝不落盘、绝不出现在日志/断言输出。
//!
//! ## 只读边界
//! 只对官方 dsh web 发只读请求：`session/list`、`session/page` 与 export HTTP
//! 路由的存在性探测（仅校验 2xx/3xx 状态即断开，不下载 body、不落盘）。
//! 不发任何写端点（prompt/cancel/rename/create/fork/selectModel/attachment/
//! feedback/skills 等一律不碰），确保冒烟本身零副作用。
//!
//! ## 断言口径（wire 已按 0.1.2-rc.1 校正）
//! - `session/list`：args `{"_request":{}}`（下划线 `_request`），响应信封
//!   `{"type":"server-response","rpcId":…,"result":{ok:true,value:{items:[…]}}}`；
//! - `session/page`：嵌套 args `{"request":{address:{kind:"session",sessionId},
//!   throughSeq,maxMessages}}`，result.ok 且 value.records 数组存在；
//! - export：`GET {base}/api/session.export?sessionId=…&includeDescendants=true`
//!   存在（2xx/3xx），不落盘。
//!
//! 实测约束（0.1.2-rc.1）：`session/page` 的 throughSeq 不能超过会话当前游标
//! （超了返回 `gateway/bad-request` "through seq … is past cursor …"），因此
//! 测试运行时先从 `session/list` 的 `projections.asOfSeq` 取目标会话当前
//! 游标（= 最大合法 throughSeq），而非写死一个任意大数。

use dshtui::api::envelope::{ClientRequest, ServerResponse};
use dshtui::api::session;
use dshtui::api::types::{meta_from_raw, SessionAddress};
use dshtui::api::DshClient;
use serde_json::{json, Value};

/// 官方 dsh web 缺省地址（REQ-008 证据后端 0.1.2-rc.1）。
const DEFAULT_BASE: &str = "http://127.0.0.1:3080";
/// REQ-008 证据会话（AC-008-14 live 契约冒烟的目标）。
const DEFAULT_SESSION: &str = "session-25536e2c-f8b9-4bcf-a16b-0baa085fa362";

/// env 门控：`DSH_TOKEN` 必填，缺失/为空 → 说明原因并返回 `None`（跳过）；
/// `DSHTUI_LIVE_BASE` / `DSHTUI_LIVE_SESSION` 有缺省。token 只在本函数内
/// 短暂持有用于连接，绝不进入日志与断言输出。
fn env_or_skip() -> Option<(String, String)> {
    let token = std::env::var("DSH_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        eprintln!(
            "[live_smoke] 跳过：DSH_TOKEN 未设置（CI 默认跳过；需真实 dsh web 时 \
             设置 DSHTUI_LIVE_SMOKE=1 并提供仓库 secret DSH_TOKEN）"
        );
        return None;
    }
    let base = std::env::var("DSHTUI_LIVE_BASE").unwrap_or_else(|_| DEFAULT_BASE.to_string());
    Some((token, base))
}

/// 目标会话 id（`DSHTUI_LIVE_SESSION`，缺省 REQ-008 证据会话）。
fn live_session_id() -> String {
    std::env::var("DSHTUI_LIVE_SESSION").unwrap_or_else(|_| DEFAULT_SESSION.to_string())
}

/// 用 lib 公开入口 `DshClient::connect` 连接并认证（token → 内存 cookie，
/// 与 CLI 同路径）；失败即 fail（env 门控已过，说明后端不可达/认证失败）。
async fn connect_live(base: &str, token: &str) -> DshClient {
    DshClient::connect(base, token)
        .await
        .expect("DshClient::connect 认证失败（后端不可达或 token 无效）")
}

/// 原始信封直连：POST `{base}/api/{method}`，返回信封 JSON（形状由调用方用
/// `ServerResponse` 断言：result.ok / rpcId 回显 / value 字段）。token 不经
/// 过本函数，错误消息只含 base/method，无凭据。
async fn raw_unary(client: &DshClient, rpc_id: &str, method: &str, args: Value) -> Value {
    let body = ClientRequest::new(rpc_id, method, args);
    let url = format!("{}/api/{method}", client.base);
    let resp = client
        .http
        .post(&url)
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{method} HTTP 传输失败（{url}）: {e}"));
    assert!(
        resp.status().is_success(),
        "{method} HTTP 状态应为 2xx，实际 {}",
        resp.status()
    );
    resp.json()
        .await
        .unwrap_or_else(|e| panic!("{method} 响应 JSON 解析失败: {e}"))
}

/// 断言信封形状 `{"type":"server-response","rpcId":…,"result":{ok:true,…}}`，
/// 返回 result.value（已断言非空）。
fn assert_envelope_ok(rpc_id: &str, method: &str, raw: &Value) -> Value {
    let sr: ServerResponse = serde_json::from_value(raw.clone())
        .unwrap_or_else(|e| panic!("{method} 信封不符合 server-response 形状: {e}"));
    assert_eq!(
        sr.kind, "server-response",
        "{method} 信封 type 应为 server-response"
    );
    assert_eq!(sr.rpc_id, rpc_id, "{method} 信封应回显 rpcId");
    assert!(
        sr.result.ok,
        "{method} result.ok 应为 true: {:?}",
        sr.result.error
    );
    sr.result
        .value
        .unwrap_or_else(|| panic!("{method} ok=true 时应携带 value"))
}

// ============================================================================
// AC-008-14 live 契约断言（全部 #[ignore] + env 门控）
// ============================================================================

/// session/list：带 cookie 的 `{"_request":{}}` → ok + value.items 数组存在；
/// 同时走 lib 强类型入口 `session::list` 断言 Ok，且 typed 解析出的 item 数
/// 与 raw 一致且非空（AC-008-14/17 response-shape drift 回归：官方 item 顶层
/// wire 名为 sessionId/updatedAt/parentSessionId，ListItemRaw 必须经 alias
/// 解析，不得把真实 item 静默丢弃成 raw_items=0）。
#[tokio::test]
#[ignore = "live 冒烟：需 DSH_TOKEN + 真实 dsh web，CI 默认跳过（env 门控）"]
async fn live_session_list_contract() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;

    // (a) 信封形状断言：raw envelope 直连（lib ClientRequest/ServerResponse）。
    let rpc_id = "live-smoke-session-list";
    let raw = raw_unary(&client, rpc_id, "session/list", json!({ "_request": {} })).await;
    let value = assert_envelope_ok(rpc_id, "session/list", &raw);
    let items = value
        .get("items")
        .unwrap_or_else(|| panic!("session/list value 应含 items: {value}"));
    assert!(items.is_array(), "session/list items 应为数组");
    let raw_items: &[Value] = items.as_array().expect("session/list items 应为数组");
    let count = raw_items.len();
    assert!(
        count > 0,
        "session/list raw items 应非空（真实 dsh web 有会话）"
    );
    println!("[live_smoke] session/list ok，raw items={count}");

    // 官方 item 顶层 wire 字段抽查（response-shape drift 锚点）。
    let first = &raw_items[0];
    assert!(
        first.get("sessionId").and_then(|v| v.as_str()).is_some(),
        "官方 item 顶层应有 sessionId: {first}"
    );
    assert!(
        first.get("updatedAt").and_then(|v| v.as_i64()).is_some(),
        "官方 item 顶层应有 updatedAt: {first}"
    );

    // (b) lib 强类型入口直接调用并断言 Ok（wire 已校正为嵌套 `_request`）。
    let page = session::list(&client.http, &client.base, None)
        .await
        .expect("session::list 强类型入口应 Ok");
    assert!(
        !page.raw_items.is_empty(),
        "typed session::list 应解析出非空 raw_items（response-shape drift 回归：官方 \
         sessionId/updatedAt 等 wire 名必须被 ListItemRaw alias 解析，不得静默丢弃）"
    );
    assert_eq!(
        page.raw_items.len(),
        count,
        "typed session::list 应解析全部 raw item（官方形状逐条可反序列化）"
    );
    // 抽查第一条：typed meta 与 raw 官方字段一致（id←sessionId、updated←updatedAt）。
    let meta = meta_from_raw(page.raw_items[0].clone()).expect("官方形状第一条应能 meta_from_raw");
    assert_eq!(
        meta.id.get(),
        first.get("sessionId").and_then(|v| v.as_str()).unwrap(),
        "meta.id 应来自官方 sessionId"
    );
    let updated = first.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
    if updated != 0 {
        assert_eq!(
            meta.updated_at_ms, updated,
            "meta.updated_at_ms 应来自官方顶层 updatedAt"
        );
    }
    // 全量 typed items 逐条经 meta_from_raw 应基本全部成功（仅空 id 会被拒，
    // live 官方不会发空 sessionId）。
    let meta_ok = page
        .raw_items
        .iter()
        .filter(|r| meta_from_raw((*r).clone()).is_some())
        .count();
    assert_eq!(
        meta_ok, count,
        "全部官方 item 都应 meta_from_raw 成功（侧栏/列表元数据来源）"
    );
    println!(
        "[live_smoke] session::list typed items={}，meta_from_raw 全量成功 {meta_ok}/{count}",
        page.raw_items.len()
    );
}

/// session/page：嵌套 `{"request":{address:{kind:"session",sessionId},throughSeq,
/// maxMessages:5}}` → ok 且 records 数组存在。throughSeq 取目标会话当前游标
/// （list 的 projections.asOfSeq）——实测 0.1.2-rc.1 拒绝超过游标的值。
#[tokio::test]
#[ignore = "live 冒烟：需 DSH_TOKEN + 真实 dsh web，CI 默认跳过（env 门控）"]
async fn live_session_page_contract() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;
    let session_id = live_session_id();

    // 1) session/list 定位目标会话，取其 projections.asOfSeq（会话当前游标 =
    //    最大合法 throughSeq；服务端 0.1.2-rc.1 对超过游标的值报 bad-request）。
    let rpc_id = "live-smoke-session-page-cursor";
    let raw = raw_unary(&client, rpc_id, "session/list", json!({ "_request": {} })).await;
    let value = assert_envelope_ok(rpc_id, "session/list", &raw);
    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("session/list value.items 应为数组: {value}"));
    let target = items
        .iter()
        .find(|item| item.get("sessionId").and_then(|v| v.as_str()) == Some(session_id.as_str()))
        .unwrap_or_else(|| {
            panic!("目标会话 {session_id} 未出现在 session/list（DSHTUI_LIVE_SESSION 配置有误？）")
        });
    let cursor = target
        .pointer("/projections/asOfSeq")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| {
            panic!("目标会话 {session_id} 缺少 projections.asOfSeq（当前游标），无法推导合法 throughSeq")
        });
    println!("[live_smoke] 目标会话 {session_id} 当前游标 asOfSeq={cursor}");

    // 2) 强类型 session::page：Ok 即证明嵌套 request 契约（address/throughSeq/
    //    maxMessages）被服务端接受且 value.records 可解析为数组。
    let address = SessionAddress::session(&session_id);
    let result = session::page(
        &client.http,
        &client.base,
        &address,
        dshtui::api::types::SessionSeq::new(cursor),
        None,
        5,
    )
    .await
    .expect("session/page 应 ok（嵌套 request 契约）");
    println!(
        "[live_smoke] session/page ok，records={}，hasMore={:?}（records 数组存在，不断言数量）",
        result.records.len(),
        result.has_more
    );
}

/// export HTTP 路由存在性：带 cookie GET
/// `{base}/api/session.export?sessionId=<sid>&includeDescendants=true` →
/// 2xx/3xx（路由应当存在，404/401 = 缺失/未认证即 fail）；不落盘、不下载
/// （仅校验状态即断开连接）。
#[tokio::test]
#[ignore = "live 冒烟：需 DSH_TOKEN + 真实 dsh web，CI 默认跳过（env 门控）"]
async fn live_export_route_exists() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;
    let session_id = live_session_id();

    let url = format!(
        "{}/api/session.export?sessionId={}&includeDescendants=true",
        client.base, session_id
    );
    let resp = client
        .http
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("export GET 传输失败（{url}）: {e}"));
    let status = resp.status().as_u16();
    assert!(
        (200..400).contains(&status),
        "export 路由应存在（期望 2xx/3xx），实际 HTTP {status}（404/401 = 路由缺失或未认证，契约应 fail）"
    );

    // 不落盘、不下载：仅校验路由状态即断开连接（reqwest 未启用 stream
    // feature，不做 body 流式消费；export 为流式下载路由，读取即产生传输）。
    drop(resp);
    println!("[live_smoke] export 路由存在：HTTP {status}（仅探测状态，未落盘）");
}

// ============================================================================
// Step E（AC-008-14 覆盖补全）：envelope 显式断言 / live follow WS（streamId
// 字符串 wire）/ projections 解析 / attachment 扫描——全部 #[ignore] + env
// 门控，只读。streamId 字符串 wire 见 src/api/mux.rs（数字 id 被官方网关拒绝
// 并断开整个 mux——Step E live 实证）。
// ============================================================================

/// 信封形状显式断言（list 已隐式覆盖，这里把核心信封字段逐项显式锁定）：
/// `{"type":"server-response","rpcId":<回显>,"result":{"ok":true,"value":…}}`。
/// 这是 Notes/03 §7.2 的核心契约——升级后若信封字段改名/缺省即在此暴露。
#[tokio::test]
#[ignore = "live 冒烟：需 DSH_TOKEN + 真实 dsh web，CI 默认跳过（env 门控）"]
async fn live_envelope_shape_explicit() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;
    let rpc_id = "live-smoke-envelope-explicit";
    let raw = raw_unary(&client, rpc_id, "session/list", json!({ "_request": {} })).await;
    let _value = assert_envelope_ok(rpc_id, "session/list", &raw);

    // 逐字段显式断言（envelope 契约锚点）。
    assert_eq!(raw["type"], "server-response", "type 字段");
    assert_eq!(raw["rpcId"], rpc_id, "rpcId 应回显");
    assert_eq!(raw["result"]["ok"], true, "result.ok");
    println!("[live_smoke] envelope 显式断言通过（rpcId 回显/type/result.ok）");
}

/// live `session/follow` WS 流（Step E / AC-008-14）：真实 mux 上开 follow，
/// 必须收到 snapshot 帧且 records 非空可解析。这是 streamId 字符串 wire 的
/// live 回归锚点。
#[tokio::test]
#[ignore = "live 冒烟：需 DSH_TOKEN + 真实 dsh web，CI 默认跳过（env 门控）"]
async fn live_session_follow_contract() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;
    let session_id = live_session_id();

    let mux = client
        .open_mux()
        .await
        .expect("mux 连接应成功（streamId 字符串 wire）");
    let address = SessionAddress::session(&session_id);
    let mut stream = session::open_follow(&mux, &address, 2)
        .await
        .expect("session/follow open 应成功（官方网关接受字符串 streamId）");

    // 首帧必须是 snapshot（含 records/header/cursor/projections 可解析）。
    let first = tokio::time::timeout(std::time::Duration::from_secs(15), stream.next())
        .await
        .expect("follow 首帧应在超时内到达")
        .expect("follow 流不应空")
        .expect("follow 首帧不应是错误");
    let parsed = dshtui::api::session::parse_follow_item(&first)
        .expect("首帧应能 parse_follow_item");
    match parsed {
        dshtui::api::session::FollowItem::Snapshot {
            cursor,
            records,
            has_more,
            projections,
        } => {
            println!(
                "[live_smoke] follow snapshot: cursor={cursor:?} records={} has_more={has_more} projections={}",
                records.len(),
                projections.is_some()
            );
            assert!(
                !records.is_empty(),
                "follow snapshot records 不应为空（真实 dsh 会话有历史）"
            );
        }
        other => panic!("follow 首帧应为 Snapshot，实际 {other:?}"),
    }
    println!("[live_smoke] follow WS ok（streamId 字符串 wire + snapshot 解析）");
}

/// projections 解析（AC-008-14 / Notes/03 §7.2）：session/list 的
/// projections.values 含 modelSelection/tokenUsage/sessionStats 等结构化字段，
/// 经宽容解析应可达（不崩）。attachment 会话覆盖：真实会话 page 记录扫描
/// attachment 型记录；无则如实标注覆盖边界（mock 兜底，不伪造通过）。
#[tokio::test]
#[ignore = "live 冒烟：需 DSH_TOKEN + 真实 dsh web，CI 默认跳过（env 门控）"]
async fn live_projections_and_attachment_scan() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;
    let session_id = live_session_id();

    // (a) 从 list 取目标会话 projections.values，抽查 modelSelection 可达。
    let list = session::list(&client.http, &client.base, None)
        .await
        .expect("session::list 应 Ok");
    let target = list.raw_items.iter().find(|r| r.id == session_id).unwrap_or_else(|| {
        panic!("目标会话 {session_id} 不在 list（DSHTUI_LIVE_SESSION 有误？）")
    });
    let proj = target
        .projections
        .as_ref()
        .expect("目标会话应有 projections");
    let values = proj.pointer("/values").expect("projections.values 应可达");
    assert!(values.is_object(), "projections.values 应为对象: {values}");
    let has_known = ["modelSelection", "tokenUsage", "sessionStats", "title"]
        .iter()
        .any(|k| values.get(*k).is_some());
    assert!(
        has_known,
        "projections.values 应含至少一个已知投影字段（modelSelection/tokenUsage/sessionStats/title）: {values}"
    );
    println!("[live_smoke] projections.values 解析可达（已知字段命中）");

    // (b) attachment 探测：page 尾部原始记录扫描 attachment 型记录
    // （page_raw 返回原始 JSON Value 供字符串级扫描；typed records 不序列化）。
    let cursor = proj.pointer("/asOfSeq").and_then(|v| v.as_u64()).unwrap_or(0);
    let address = SessionAddress::session(&session_id);
    let page = session::page_raw(
        &client.http,
        &client.base,
        &address,
        dshtui::api::types::SessionSeq::new(cursor),
        None,
        200,
    )
    .await
    .expect("session/page_raw 应 Ok");
    let mut att = 0usize;
    for r in &page.records {
        let s = serde_json::to_string(r).unwrap_or_default();
        if s.contains("\"attachment\"") || s.contains("image/") {
            att += 1;
        }
    }
    if att == 0 {
        println!(
            "[live_smoke] attachment：目标会话 page 尾部未发现 attachment 记录——标注覆盖边界，mock 兜底"
        );
    } else {
        println!("[live_smoke] attachment：page 尾部发现 {att} 条 attachment/image 记录");
    }
}
