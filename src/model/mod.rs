//! model layer: windowed transcript + projection snapshot + lightweight
//! metadata.
//!
//! Model types do not depend on reqwest/ratatui (pure sync, table-driven
//! testable); the single maintenance point for seq/requestId/window
//! capacity/anchor (Step 3 is the correctness seam).

pub mod agent_roster;
pub mod agent_town;
pub mod approval;
pub mod catalog;
pub mod draft;
pub mod draft_store;
pub mod export;
pub mod external_edit;
pub mod goal;
pub mod image;
pub mod image_attachment;
pub mod jobs;
pub mod kb_stats;
pub mod mention;
pub mod message_action;
pub mod plan;
pub mod projections;
pub mod search;
pub mod session;
pub mod settings;
pub mod skills;
pub mod subagent;
pub mod theme;
pub mod timeline;

pub mod trajectory;

pub mod sprite_tables;
pub mod workspace;
pub mod yank;

pub use agent_roster::{
    esc, fmt_elapsed, short_session, stage_key, stage_meta, AgentKind, AgentRosterEntry,
    AgentStatus, RosterSnapshot, StageMeta, STAGES,
};
pub use approval::{ApprovalItem, ApprovalQueue, ApprovalSummary};
pub use catalog::{CatalogIndex, ModelCatalogItem};
pub use draft::{DraftRegistry, DraftState, InputHistory};
pub use draft_store::DraftStore;
pub use export::ExportState;
pub use external_edit::ExternalEditState;
pub use goal::{GoalOpKind, GoalPanelState, GoalView};
pub use image::{
    image_block_of, is_supported_image, AttachmentRef, ImageBlockRef, ImageCacheEntry,
    ImageViewError, ImageViewPhase, ImageViewState, OriginalDimensions, SUPPORTED_IMAGE_TYPES,
};
pub use image_attachment::{media_type_from_path, ImageAttachment, ImageAttachmentState};
pub use jobs::JobsPanelState;
pub use kb_stats::{KbBucket, KbHistogram, KbStatsError, KbStatsSnapshot, KB_DURATION_BOUNDARIES};
pub use mention::{MentionCandidate, MentionKind, MentionState};
pub use message_action::{can_branch, MessageActionKind, MessageActionState};
pub use plan::PlanViewState;
pub use projections::ProjectionSnapshot;
pub use search::{
    SearchHistory, SearchIndex, SearchItem, SearchKind, SearchKindFilter, SearchMatch,
};
pub use session::{
    ApplyEffect, Block, Incoming, PackedChunks, PendingEcho, PendingEchoStatus, SessionStore,
    TranscriptWindow, TurnOutlineItem,
};
pub use settings::{coerce_edit, flatten_namespace_rows, SettingsPanelState, SettingsRow};
pub use skills::SkillsCatalogState;
pub use subagent::{
    lineage_breadcrumbs, LineageCrumb, SubagentNode, SubagentNodeRef, SubagentViewState,
};
pub use theme::ThemeKeymapConfig;
pub use timeline::{TimelineMarker, TimelineMarkerKind, TimelineState};
pub use trajectory::{
    detail_for, kind_label, FoldState, GroupId, RowId, TrajEffect, TrajError, TrajIncoming,
    TrajKind, TrajTiming, TrajUsage, TrajectoryDetail, TrajectoryRow, TrajectorySearchIndex,
    TrajectorySearchItem, TrajectorySearchMatch, TrajectoryStore, TrajectoryWindow,
};
pub use workspace::{
    sidebar_rows, GroupBy, OrderBy, SidebarRow, WorkspaceMeta, WorkspaceStore, WorkspaceViewState,
};
pub use yank::{
    block_plain_text, block_yank_target, selection_text, VisualMode, VisualSelection, YankBackend,
    YankState, YankTarget,
};
