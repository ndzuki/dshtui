//! dshtui — 官方 dsh web Remote API 的 Rust TUI 客户端（V0.1 核心底座）。
//!
//! 分层（Notes/02 §2）：`api`（协议客户端）→ `model`（窗口化模型）→ `app`
//! （AppState/帧循环）→ `ui`/`input`（渲染与键位）。所有状态变更经 mpsc
//! 进入单一 `AppState`（单写多读）。

pub mod api;
pub mod app;
pub mod config;
pub mod input;
pub mod model;
pub mod ui;
