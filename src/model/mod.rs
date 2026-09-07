//! model layer: windowed transcript + projection snapshot + lightweight
//! metadata.
//!
//! Model types do not depend on reqwest/ratatui (pure sync, table-driven
//! testable); the single maintenance point for seq/requestId/window
//! capacity/anchor (Step 3 is the correctness seam).

pub mod draft;
pub mod image;
pub mod projections;
pub mod search;
pub mod session;
pub mod trajectory;
pub mod workspace;
pub mod yank;

pub use draft::{DraftRegistry, DraftState, InputHistory};
pub use image::{
    image_block_of, is_supported_image, AttachmentRef, ImageBlockRef, ImageCacheEntry,
    ImageViewError, ImageViewPhase, ImageViewState, OriginalDimensions, SUPPORTED_IMAGE_TYPES,
};
pub use projections::ProjectionSnapshot;
pub use search::{SearchIndex, SearchItem, SearchKind, SearchKindFilter, SearchMatch};
pub use session::{
    ApplyEffect, Block, Incoming, PackedChunks, PendingEcho, PendingEchoStatus, SessionStore,
    TranscriptWindow, TurnOutlineItem,
};
pub use trajectory::{
    detail_for, kind_label, FoldState, GroupId, RowId, TrajEffect, TrajError, TrajIncoming,
    TrajKind, TrajTiming, TrajUsage, TrajectoryDetail, TrajectoryRow, TrajectorySearchIndex,
    TrajectorySearchItem, TrajectorySearchMatch, TrajectoryStore, TrajectoryWindow,
};
pub use workspace::{WorkspaceMeta, WorkspaceStore};
pub use yank::{
    block_plain_text, block_yank_target, selection_text, VisualMode, VisualSelection, YankBackend,
    YankState, YankTarget,
};
