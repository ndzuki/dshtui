//! model 层：窗口化转录本 + 投影快照 + 轻量元数据。
//!
//! 模型类型不依赖 reqwest/ratatui（纯同步、可表驱动测试）；
//! seq/requestId/窗口容量/anchor 的唯一维护点（Step 3 是正确性 seam）。

pub mod projections;
pub mod session;
pub mod workspace;

pub use projections::ProjectionSnapshot;
pub use session::{
    ApplyEffect, Block, Incoming, PackedChunks, SessionStore, TranscriptWindow, TurnOutlineItem,
};
pub use workspace::{WorkspaceMeta, WorkspaceStore};
