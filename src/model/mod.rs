//! model layer: windowed transcript + projection snapshot + lightweight
//! metadata.
//!
//! Model types do not depend on reqwest/ratatui (pure sync, table-driven
//! testable); the single maintenance point for seq/requestId/window
//! capacity/anchor (Step 3 is the correctness seam).

pub mod projections;
pub mod search;
pub mod session;
pub mod workspace;

pub use projections::ProjectionSnapshot;
pub use search::SearchIndex;
pub use session::{
    ApplyEffect, Block, Incoming, PackedChunks, SessionStore, TranscriptWindow, TurnOutlineItem,
};
pub use workspace::{WorkspaceMeta, WorkspaceStore};
