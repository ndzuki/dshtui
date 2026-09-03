//! workspace/* 端点封装（V0.1 子集：follow，Notes/03 §3）。
//!
//! workspace/follow 的帧形状 Notes/03 未做字段级记录，本层按容忍策略解析：
//! 已知形态（`{type:"snapshot", workspaces:[...]}` / `{workspaces:[...]}` / 平铺列表）
//! 直接解析，未知形态保留原始帧并计入 tracing（不静默丢弃）。

use serde_json::Value;

use super::envelope::ClientError;
use super::mux::{Mux, StreamHandle};

pub async fn open_follow(mux: &Mux) -> Result<StreamHandle, ClientError> {
    mux.open_stream("workspace/follow", serde_json::json!({}))
        .await
}

/// 从 follow 帧提取 workspace 原始列表（容忍多形态；识别不了返回 None 并记日志）。
pub fn extract_workspaces(frame: &Value) -> Option<Vec<Value>> {
    let v = frame.get("workspaces").or_else(|| frame.get("items"));
    if let Some(arr) = v.and_then(|x| x.as_array()) {
        return Some(arr.clone());
    }
    if let Some(arr) = frame.as_array() {
        return Some(arr.clone());
    }
    tracing::warn!("workspace/follow 帧形态无法识别，保留原始帧");
    None
}
