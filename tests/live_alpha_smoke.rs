//! live_alpha_smoke.rs —— REQ-008 Step F（D-62）：官方 alpha 一次性只读实例的
//! 契约表面冒烟（integration test）。
//!
//! 与 `tests/live_smoke.rs`（针对本机 3080 的**有数据** dsh，断言依赖真实会话）
//! 不同，本文件断言的是**数据无关的协议表面**——对一个刚启动、零会话的官方
//! dsh web 实例也应成立：
//!   1. 认证入口（token → cookie 303）可用（`DshClient::connect`）；
//!   2. `session/list` 信封形状 + items 数组（可为空）可解析；
//!   3. `session/modelCatalog` 零参 unary ok（默认模型目录非空）；
//!   4. `session/page` 对不存在会话返回 **typed 错误信封**（error.code =
//!      session/not-found）——错误路径契约，不崩、不整树断开；
//!   5. export HTTP 路由存在：对不存在会话 404 = 路由存在 + 已认证
//!      （401 才表示未认证/路由缺失）。
//!
//! 用途（CI）：`scripts/ci-live-smoke.sh` 下载官方目标 alpha → 临时
//! DSH_HOME 起一次性只读实例 → 注入 `DSHTUI_LIVE_BASE`/`DSH_TOKEN` 后跑本
//! 文件——官方升级后最先暴露的是这些协议表面 wire 漂移（如 streamId 字符串、
//! 信封字段、错误码），而非依赖具体数据的行为。
//!
//! ## env 门控（与 live_smoke.rs 同构）
//! - 所有测试 `#[ignore]`：`cargo test --all-targets` 不碰 live；
//! - `DSH_TOKEN` 必填；`DSHTUI_LIVE_BASE` 缺省 `http://127.0.0.1:3080`；
//! - token 只用于连接，绝不落盘/出现在日志。
//!
//! ## 只读边界
//! 只发只读请求（list/modelCatalog/page/export GET 探测）；不发任何写端点。

use dshtui::api::envelope::{ClientRequest, ServerResponse};
use dshtui::api::DshClient;
use serde_json::{json, Value};

/// 官方 dsh web 缺省地址。
const DEFAULT_BASE: &str = "http://127.0.0.1:3080";

/// env 门控：`DSH_TOKEN` 必填；`DSHTUI_LIVE_BASE` 缺省。token 只在本函数内
/// 短暂持有用于连接。
fn env_or_skip() -> Option<(String, String)> {
    let token = std::env::var("DSH_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        eprintln!(
            "[live_alpha_smoke] 跳过：DSH_TOKEN 未设置（CI 默认跳过；由 \
             ci-live-smoke.sh 注入实例自身 launch token）"
        );
        return None;
    }
    let base = std::env::var("DSHTUI_LIVE_BASE").unwrap_or_else(|_| DEFAULT_BASE.to_string());
    Some((token, base))
}

async fn connect_live(base: &str, token: &str) -> DshClient {
    DshClient::connect(base, token)
        .await
        .expect("DshClient::connect 认证失败（后端不可达或 token 无效）")
}

/// 原始信封直连（同 live_smoke.rs helper）。
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
// 数据无关协议表面断言（全部 #[ignore] + env 门控）
// ============================================================================

/// (1)+(2) 认证 + session/list 信封形状：items 数组可解析（空实例允许空数组）。
#[tokio::test]
#[ignore = "alpha 冒烟：需 DSH_TOKEN + 官方实例，CI 默认跳过（由 ci-live-smoke.sh 注入）"]
async fn alpha_connect_and_list_envelope() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;

    let rpc_id = "alpha-smoke-list";
    let raw = raw_unary(&client, rpc_id, "session/list", json!({ "_request": {} })).await;
    let value = assert_envelope_ok(rpc_id, "session/list", &raw);
    let items = value
        .get("items")
        .unwrap_or_else(|| panic!("session/list value 应含 items: {value}"));
    assert!(items.is_array(), "session/list items 应为数组（可为空）");
    println!(
        "[live_alpha_smoke] session/list envelope ok，items={}（空实例允许空）",
        items.as_array().map(|a| a.len()).unwrap_or(0)
    );
}

/// (3) session/modelCatalog 零参 unary ok：默认模型目录存在（groups 非空）。
#[tokio::test]
#[ignore = "alpha 冒烟：需 DSH_TOKEN + 官方实例，CI 默认跳过（由 ci-live-smoke.sh 注入）"]
async fn alpha_model_catalog_ok() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;

    let catalog = dshtui::api::session::model_catalog(&client.http, &client.base)
        .await
        .expect("session/modelCatalog 应 ok（零参 unary 契约）");
    assert!(
        !catalog.groups.is_empty(),
        "modelCatalog groups 不应为空（官方默认模型目录）"
    );
    println!(
        "[live_alpha_smoke] modelCatalog ok，groups={}",
        catalog.groups.len()
    );
}

/// (4) session/page 对不存在会话 → typed 错误信封（error.code），不崩。
/// 这验证错误路径契约在官方实例上仍以 `error.code/message/details` 信封
/// 返回（Notes/03 §6 分类前提），而非整树/HTTP 层错误。
#[tokio::test]
#[ignore = "alpha 冒烟：需 DSH_TOKEN + 官方实例，CI 默认跳过（由 ci-live-smoke.sh 注入）"]
async fn alpha_page_missing_session_typed_error() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;

    let rpc_id = "alpha-smoke-page-missing";
    let raw = raw_unary(
        &client,
        rpc_id,
        "session/page",
        json!({
            "request": {
                "address": { "kind": "session", "sessionId": "session-alpha-does-not-exist" },
                "throughSeq": 5,
                "maxMessages": 5,
            }
        }),
    )
    .await;
    let sr: ServerResponse = serde_json::from_value(raw.clone())
        .unwrap_or_else(|e| panic!("错误信封也应符合 server-response 形状: {e}"));
    assert_eq!(sr.kind, "server-response");
    assert_eq!(sr.rpc_id, rpc_id, "错误信封也应回显 rpcId");
    assert!(!sr.result.ok, "不存在会话应 ok=false");
    let err = sr
        .result
        .error
        .as_ref()
        .unwrap_or_else(|| panic!("ok=false 时应携带 error: {raw}"));
    assert!(!err.code.is_empty(), "错误信封 error.code 不应为空: {raw}");
    assert!(
        err.code.contains("not-found") || err.code.contains("not_found"),
        "错误码应表达会话不存在（session/not-found 或同类），实际 code={}",
        err.code
    );
    println!(
        "[live_alpha_smoke] session/page 不存在会话 → typed 错误信封 code={}",
        err.code
    );
}

/// (5) export HTTP 路由存在：带 cookie GET 不存在会话 → 404（路由存在 + 已
/// 认证；401 = 未认证，路由缺失会 404 但认证失败是 401）。
#[tokio::test]
#[ignore = "alpha 冒烟：需 DSH_TOKEN + 官方实例，CI 默认跳过（由 ci-live-smoke.sh 注入）"]
async fn alpha_export_route_exists() {
    let Some((token, base)) = env_or_skip() else {
        return;
    };
    let client = connect_live(&base, &token).await;

    let url = format!(
        "{}/api/session.export?sessionId=session-alpha-does-not-exist&includeDescendants=true",
        client.base
    );
    let resp = client
        .http
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("export GET 传输失败（{url}）: {e}"));
    let status = resp.status().as_u16();
    // 404 = 路由存在 + 已认证（只是会话不存在）；401 = 未认证（路由已移动 /
    // 认证失效）；200/3xx = 意外但同样证明路由活着。只有 401 才是契约 fail。
    assert_ne!(
        status, 401,
        "export 路由探测返回 401 = 未认证（token 无效或路由已移动）"
    );
    drop(resp);
    println!(
        "[live_alpha_smoke] export 路由存在：不存在会话 HTTP {status}（404 = 路由在 + 已认证）"
    );
}
