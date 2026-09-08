//! Strongly-typed newtypes and wire structs (Notes/03 §4/§5/§7 compatibility
//! action #1).
//!
//! Compatibility strategy (Notes/03 §7): unknown fields are always tolerated
//! (`#[serde(default)]`), unknown event types are skipped but logged. Field
//! naming follows the official Remote API as actually read (D-3=A).

use serde::{Deserialize, Serialize};

// ---------- newtypes (aligned with the official SessionSeq/SessionLogOffset
// strong typing) ----------

macro_rules! newtype {
    ($name:ident, $inner:ty, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
        )]
        #[serde(transparent)]
        pub struct $name(pub $inner);

        impl $name {
            pub fn new(v: $inner) -> Self {
                Self(v)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

newtype!(
    SessionId,
    String,
    "Session identifier (official session id, string)"
);
newtype!(WorkspaceId, String, "Workspace / project identifier");
newtype!(SessionSeq, u64, "Event sequence number (monotonic)");
newtype!(
    SessionLogOffset,
    u64,
    "Log offset (page/follow cursor semantics)"
);
newtype!(
    RequestId,
    String,
    "Request idempotency key (D-4: repeated delivery applies idempotently)"
);
newtype!(
    SessionRequestId,
    String,
    "Client-minted prompt identity (brand `session-request-id`; persisted on the accepted user message source)"
);
newtype!(
    AttachmentId,
    String,
    "Attachment identifier (opaque id — never a filesystem path or URL, REQ-004 §7)"
);
newtype!(
    MediaType,
    String,
    "Attachment media type (image/png | image/jpeg | image/webp | image/gif whitelist)"
);

/// Mint a new session request idempotency key: same pid+counter style as the
/// api-layer rpcId (no UUID crate added).
pub fn mint_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!(
        "dshtui-req-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

// Numeric newtypes allow Copy (the window reducer copies them heavily; avoids
// move noise).
impl Copy for SessionSeq {}
impl Copy for SessionLogOffset {}

// ---------- session/page / session/follow address ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionAddress {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

impl SessionAddress {
    pub fn session(id: &str) -> Self {
        Self {
            kind: "session".into(),
            session_id: Some(id.to_string()),
            parent_session_id: None,
            child_session_id: None,
            mode: None,
        }
    }
}

// ---------- session/prompt request types (REQ-002 §3; official fields read
// 2026-09-04 from @deepseek-ai/dsh-api-session-controller@0.1.2-alpha.5) ----------

/// `session/prompt` mode: V0.1 only constructs `Queue` (including sending
/// while running); `Steer` is the V0.2/REQ-003 extension slot (D-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    Queue,
    Steer,
}

/// Prompt content part (official shape `{type:"text",text}`; the `image` part
/// belongs to V0.4 and is only tolerated on the wire here, never constructed
/// in V0.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PromptContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        #[serde(rename = "mediaType")]
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// `session/prompt` request args (= envelope `payload.args`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// Client-minted idempotency key persisted on the accepted user message
    /// source (`user-rpc.rpcId`); reconciles optimistic echo with durable
    /// events (AC-002-06).
    pub request_id: SessionRequestId,
    pub session_id: SessionId,
    pub mode: PromptMode,
    pub content: Vec<PromptContentPart>,
    /// Optional IANA tz; never sent in V0.1 (omitted when None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_time_zone: Option<String>,
}

// ---------- SessionHistoryRecord (Notes/03 §4.3, two variants) ----------

/// `{type:"event", event:{...}}` or `{type:"chunks", event:{chunkrow}}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionHistoryRecord {
    #[serde(rename = "event")]
    Event { event: SessionWireEvent },
    #[serde(rename = "chunks")]
    Chunks { event: ChunkRow },
}

/// Event record: type/seq/time/data + requestId (D-4 idempotency key) +
/// ignorable flag, etc.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWireEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub seq: Option<SessionSeq>,
    #[serde(default)]
    pub time: Option<i64>,
    /// Idempotency key: duplicate deliveries with the same requestId apply
    /// only once (REQ-002 AC-002-06 base contract).
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub ignorable: Option<bool>,
    #[serde(default)]
    pub source_event_seqs: Option<Vec<u64>>,
    #[serde(default)]
    pub surface_op: Option<String>,
    /// Raw payload: unknown blocks are preserved as-is (D-4), never parsed,
    /// never dropped.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

impl SessionWireEvent {
    /// Reconciliation requestId of a durable event: prefer the top-level
    /// `requestId` (existing envelope field); fall back to the official user
    /// event source (`user-rpc.rpcId`, MessageSourceMap read 2026-09-04).
    /// Protocol shape knowledge lives here in the api layer.
    pub fn reconcile_id(&self) -> Option<&str> {
        if let Some(rid) = self.request_id.as_deref() {
            return Some(rid);
        }
        if self.event_type.starts_with("user/") {
            return self
                .data
                .as_ref()
                .and_then(|d| d.get("source"))
                .and_then(|s| s.get("user-rpc"))
                .and_then(|u| u.get("rpcId"))
                .and_then(|v| v.as_str());
        }
        None
    }
}

/// packed chunk row (stored as-is, not expanded into per-delta items — key to
/// the official low-memory optimization).
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkRow {
    TextChunks(ChunkData),
    ReasoningChunks(ChunkData),
    ToolCallChunks(ToolCallChunkData),
    /// Unknown chunkrow type: preserve both its type and complete raw payload.
    Unknown {
        event_type: String,
        raw: serde_json::Value,
    },
}

impl<'de> Deserialize<'de> for ChunkRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let Some(event_type) = raw.get("type").and_then(|v| v.as_str()) else {
            return Ok(Self::Unknown {
                event_type: "<missing>".into(),
                raw,
            });
        };
        match event_type {
            "chunkrow/text-chunks" => serde_json::from_value::<ChunkData>(raw)
                .map(Self::TextChunks)
                .map_err(serde::de::Error::custom),
            "chunkrow/reasoning-chunks" => serde_json::from_value::<ChunkData>(raw)
                .map(Self::ReasoningChunks)
                .map_err(serde::de::Error::custom),
            "chunkrow/tool-call-chunks" => serde_json::from_value::<ToolCallChunkData>(raw)
                .map(Self::ToolCallChunks)
                .map_err(serde::de::Error::custom),
            _ => Ok(Self::Unknown {
                event_type: event_type.to_string(),
                raw,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChunkData {
    #[serde(default)]
    pub texts: Vec<String>,
    #[serde(default)]
    pub turn: Option<u64>,
    #[serde(default)]
    pub step: Option<u64>,
    #[serde(default)]
    pub index: Option<u64>,
    #[serde(default)]
    pub dt: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallChunkData {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(default)]
    pub turn: Option<u64>,
    #[serde(default)]
    pub step: Option<u64>,
    #[serde(default)]
    pub index: Option<u64>,
    #[serde(default)]
    pub dt: Vec<f64>,
}

// ---------- session/follow frames (Notes/03 §4.2) ----------

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum FollowFrame {
    /// Note: for serde internally-tagged enums, variant fields are not
    /// affected by the enum-level rename_all; every field must be renamed
    /// explicitly (camelCase protocol shape).
    #[serde(rename = "snapshot")]
    Snapshot {
        #[serde(default)]
        header: Option<serde_json::Value>,
        #[serde(default)]
        cursor: Option<SessionLogOffset>,
        #[serde(default)]
        records: Vec<SessionHistoryRecord>,
        #[serde(default, rename = "hasMore")]
        has_more: Option<bool>,
        #[serde(default)]
        projections: Option<serde_json::Value>,
    },
    #[serde(rename = "event")]
    Event { event: SessionWireEvent },
}

// ---------- session/page response (Notes/03 §4.1) ----------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PageResult {
    #[serde(default)]
    pub records: Vec<SessionHistoryRecord>,
    #[serde(default)]
    pub has_more: Option<bool>,
}

// ---------- session/search (REQ-003 §3; official read 0.1.2-rc.1) ----------

/// One session-level search hit (`{sessionId, snippet}` — NO seq/turn,
/// server truncates the snippet at ≤240 code points).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub session_id: SessionId,
    #[serde(default)]
    pub snippet: String,
}

/// `session/search` result: at most 20 items, `hasMore` only hints to narrow
/// the query (there is no paging RPC).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    #[serde(default)]
    pub items: Vec<SearchHit>,
    #[serde(default)]
    pub has_more: bool,
}

// ---------- session/control stream items (REQ-003 §3) ----------

/// One parsed `session/control` frame. The wire shapes of the queue/jobs/
/// projection replacement frames are not fully verified; unknown payloads are
/// preserved raw and only the `projections.running` fact is consumed.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlItem {
    /// First frame of the stream (`SessionControlBaseline`).
    Baseline {
        queues: serde_json::Value,
        jobs: serde_json::Value,
        projections: serde_json::Value,
        raw: serde_json::Value,
    },
    /// Replacement frames (queue/jobs/projection).
    Queue {
        queue: serde_json::Value,
    },
    Jobs {
        jobs: serde_json::Value,
    },
    Projection {
        projection: serde_json::Value,
    },
    /// Unknown frame kind: preserved, never dropped silently.
    Unknown {
        kind: String,
        raw: serde_json::Value,
    },
}

// ---------- approval (REQ-003 §3/§7; D-18 event source correction) ----------

/// Official `ApprovalOutcome` vocabulary (`@deepseek-ai/dsh-user-approval`,
/// read 0.1.2-rc.1). `allowed-once` is the only authorizing value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

impl ApprovalOutcome {
    /// Wire literal for display/logging.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalOutcome::AllowedOnce => "allowed-once",
            ApprovalOutcome::Rejected => "rejected",
            ApprovalOutcome::Cancelled => "cancelled",
            ApprovalOutcome::Unavailable => "unavailable",
        }
    }
}

/// A forwarded `approval/request` waterfall event. Only the identity keys are
/// strongly typed; the full payload (tool/command/reason/workspace fields) is
/// preserved raw because the field names are `[未验证]` (contract smoke locks
/// them, AC-003-18 fallback otherwise).
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalEvent {
    pub client_id: String,
    pub event_id: String,
    pub raw: serde_json::Value,
}

// ---------- REQ-006 model catalog / commands / workspace / session mutation
// wire types (V0.3; official read 0.1.2-rc.1 from typert.remote-client.js) ----------

/// Official `ModelSelection` wire shape (`{provider, model, reasoningEffort?}`).
/// The status bar models both this and the legacy string form (`Notes/03 §5`
/// alpha.3), so reads are tolerant (see `model_selection_display`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WireModelSelection {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl WireModelSelection {
    /// `provider/model` display string (empty when unset).
    pub fn display(&self) -> String {
        if self.provider.is_empty() {
            self.model.clone()
        } else if self.model.is_empty() {
            self.provider.clone()
        } else {
            format!("{}/{}", self.provider, self.model)
        }
    }
}

/// One model entry inside a provider group.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogModel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Optional reasoning metadata for this exact route (`efforts` + default).
    #[serde(default)]
    pub reasoning: Option<ModelReasoning>,
}

/// Adapter-owned reasoning metadata for one model route.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelReasoning {
    #[serde(default)]
    pub efforts: Vec<ModelReasoningEffort>,
    #[serde(default)]
    pub default_effort: Option<String>,
}

/// One selectable reasoning effort (id/name/description).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelReasoningEffort {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// One provider group with its loaded models.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderGroup {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub models: Vec<ModelCatalogModel>,
}

/// A provider whose catalog lookup failed (shown as a collapsed failure row).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogFailure {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub message: String,
}

/// `session/modelCatalog` response (`default`, `routableProviders`, `groups`,
/// `failures`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    #[serde(default)]
    pub default: Option<WireModelSelection>,
    #[serde(default)]
    pub routable_providers: Vec<String>,
    #[serde(default)]
    pub groups: Vec<ModelProviderGroup>,
    #[serde(default)]
    pub failures: Vec<ModelCatalogFailure>,
}

/// `session/fork` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkValue {
    #[serde(default)]
    pub session_id: String,
}

/// `session/rename` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionRenameValue {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub seq: i64,
}

/// `session/create` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateValue {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub agent_preset: Option<String>,
}

/// `commands/list` one command descriptor (`{name, description, input?}`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommandDescriptor {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input: Option<CommandInputSpec>,
}

/// `commands/list` per-command `input` spec.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommandInputSpec {
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub images: Option<bool>,
}

/// `commands/execute` result (may be `undefined` on the wire when the server
/// decides there is nothing to report; the api layer tolerates that and maps
/// it to a success with no text).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecution {
    #[serde(default)]
    pub command_id: String,
    #[serde(default)]
    pub result: Option<CommandExecutionResult>,
}

/// One `commands/execute` result body (`{kind: success|error, text?,
/// sourceEventSeq?}`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionResult {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub source_event_seq: Option<i64>,
}

// ---------- session list (session/list lightweight metadata, Notes/06 §1) ----------

/// The TUI keeps only a lightweight structure (~500B/item); the rest of the
/// projections are discarded.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: SessionId,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub updated_at_ms: i64,
    pub running: bool,
    pub blank: bool,
    pub origin: Option<String>,
    pub parent_id: Option<SessionId>,
    pub workspace: Option<WorkspaceId>,
    pub last_turn_preview: Option<String>,
}

/// Raw `session/list` entry.
///
/// 官方 wire 形状（0.1.2-rc.1 live 实证，typert.remote-client.d.ts
/// SessionSummary）：顶层 `sessionId / updatedAt / running / blank /
/// parentSessionId? / origin? / cwd? / projections?`，全部 camelCase 且与
/// Rust 字段名不同（`sessionId`→`id`、`updatedAt`→`updated_at_ms`、
/// `parentSessionId`→`parent_id`），因此每个官方字段用逐字段 `alias` 声明，
/// 同时保留自然键/旧 mock 形状（`id`/`updatedAtMs`/`updated_at_ms`/
/// `parentId`/`parent_id`）兼容。title/cwd/updated_at/running/blank 等另有
/// projections.values 兜底（含无 projections 的浅 item 顶层直读）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListItemRaw {
    /// 官方 wire `sessionId`；旧形状顶层 `id` 兼容。
    #[serde(alias = "sessionId")]
    pub id: String,
    #[serde(default)]
    pub workspace: Option<String>,
    /// 官方 wire `parentSessionId`；`parentId`/`parent_id` 兼容。
    #[serde(default, alias = "parentSessionId", alias = "parent_id")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub projections: Option<serde_json::Value>,
    // Direct-field fallback (still readable when the official shape changes).
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// 官方 wire `updatedAt`（epoch ms，每个 item 都有）；`updatedAtMs`/
    /// `updated_at_ms` 兼容。
    #[serde(default, alias = "updatedAt", alias = "updated_at_ms")]
    pub updated_at_ms: Option<i64>,
    #[serde(default)]
    pub running: Option<bool>,
    #[serde(default)]
    pub blank: Option<bool>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub last_turn_preview: Option<String>,
}

/// Extract lightweight metadata from a raw entry (tolerates missing fields; a
/// missing id is treated as an invalid row).
pub fn meta_from_raw(raw: ListItemRaw) -> Option<SessionMeta> {
    if raw.id.is_empty() {
        return None;
    }
    // projections shape: {"values": {title, ...}}; sessionListMetadata
    // provides lastPromptAt/blank.
    let vals = raw
        .projections
        .as_ref()
        .and_then(|p| p.get("values"))
        .or(raw.projections.as_ref());
    let get_str = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            vals.and_then(|v| v.get(k))
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
    };
    let get_bool = |keys: &[&str]| -> Option<bool> {
        keys.iter()
            .find_map(|k| vals.and_then(|v| v.get(k)).and_then(|x| x.as_bool()))
    };
    let metadata = vals.and_then(|v| v.get("sessionListMetadata"));
    // 主源：官方顶层 `updatedAt`（alias 后 raw.updated_at_ms 直接可读，浅 item
    // 也有）；旧兜底：projections.values.sessionListMetadata.lastPromptAt
    // （与 updatedAt 同值，live 实证；保留兼容旧 mock）。
    let updated_at_ms = raw
        .updated_at_ms
        .or_else(|| {
            metadata
                .and_then(|m| m.get("lastPromptAt"))
                .and_then(|x| x.as_i64())
        })
        .unwrap_or(0);

    // Summary of the last turnOutline response (≤120 chars, official
    // truncation policy).
    let last_turn_preview = raw.last_turn_preview.clone().or_else(|| {
        vals.and_then(|v| v.get("turnOutline"))
            .and_then(|x| x.as_array())
            .and_then(|a| a.last())
            .and_then(|t| t.get("response"))
            .and_then(|r| r.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(120).collect())
    });

    Some(SessionMeta {
        id: SessionId(raw.id),
        title: get_str(&["title"]).or(raw.title.clone()),
        cwd: get_str(&["cwd"]).or(raw.cwd.clone()),
        updated_at_ms,
        running: get_bool(&["running"]).or(raw.running).unwrap_or(false),
        blank: get_bool(&["blank"])
            .or_else(|| {
                metadata
                    .and_then(|m| m.get("blank"))
                    .and_then(|x| x.as_bool())
            })
            .or(raw.blank)
            .unwrap_or(false),
        origin: raw.origin.clone(),
        parent_id: raw.parent_id.map(SessionId),
        workspace: raw.workspace.map(WorkspaceId),
        last_turn_preview,
    })
}

// ---------- REQ-007 V0.4 wire types (official read 0.1.2-rc.1; contract
// smoke locks remaining `[未验证]` fields; all unknown fields tolerated) ----------

impl SessionAddress {
    /// Subagent address (`kind:"subagent"`): parent + child + mode.
    pub fn subagent(parent: &str, child: &str, mode: &str) -> Self {
        Self {
            kind: "subagent".into(),
            session_id: None,
            parent_session_id: Some(parent.to_string()),
            child_session_id: Some(child.to_string()),
            mode: Some(mode.to_string()),
        }
    }
}

// ---------- subagents/* ----------

/// `subagents/list(parentSessionId)` catalog (direct children only; the tree
/// must recurse per parent guided by `hasChildren`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentCatalog {
    #[serde(default)]
    pub entries: Vec<SubagentListEntry>,
    #[serde(default)]
    pub parent_available: bool,
}

/// One `subagents/list` entry (child / diagnostic variant; unknown kinds are
/// preserved tolerantly as raw).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SubagentListEntry {
    #[serde(rename = "child", rename_all = "camelCase")]
    Child {
        id: String,
        #[serde(default)]
        activity: String,
        #[serde(default)]
        has_children: bool,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    #[serde(rename = "diagnostic", rename_all = "camelCase")]
    Diagnostic {
        id: String,
        #[serde(default)]
        reason: String,
    },
}

/// `subagents/prompt` request args (single `request`-parameter endpoint).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentPromptRequest {
    pub request_id: String,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub mode: String,
    pub content: Vec<PromptContentPart>,
}

/// `subagents/prompt` / `subagents/interruptByParent` receipt.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentReceipt {
    #[serde(default)]
    pub accepted: Option<bool>,
    #[serde(default)]
    pub message_id: Option<String>,
}

// ---------- goals/* (per-session singleton; agentId first flat param) ----------

/// Goal phase vocabulary (projection `goal.phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalPhase {
    Active,
    Paused,
    Blocked,
    Complete,
}

/// `goals/*` mutation receipt (`{ref:{id,revision}}`) — also the CAS payload
/// (`revision` is echoed back on every mutation).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRef {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub revision: u64,
}

/// One goal snapshot (also embedded in the `goal` projection).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSnapshot {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub phase: Option<GoalPhase>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    #[serde(default)]
    pub max_goal_rounds: Option<u64>,
}

/// `goals/create` request args (single `request`-parameter endpoint).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGoalRequest {
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_goal_rounds: Option<u64>,
}

// ---------- settings/* ----------

/// `settings/describe` value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDescribeValue {
    #[serde(default)]
    pub writable: bool,
    #[serde(default)]
    pub has_document: bool,
    #[serde(default)]
    pub namespaces: Vec<SettingsNamespaceView>,
}

/// One settings namespace view (describe/update/replace result).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsNamespaceView {
    #[serde(default)]
    pub ns: String,
    #[serde(default)]
    pub schema: serde_json::Value,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default)]
    pub base: Option<serde_json::Value>,
    #[serde(default)]
    pub user: Option<serde_json::Value>,
    #[serde(default)]
    pub applies: String,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub revision: u64,
}

/// One `settings/mutate` path operation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPathOpView {
    pub path: String,
    #[serde(rename = "op")]
    pub op_kind: String, // "set" | "unset"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

// ---------- skills/* ----------

/// `skills/list(sessionId)` value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillListValue {
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
}

/// One skill entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub model_invocable: bool,
}

// ---------- @ mentions (fileReferences + sessionReferenceResolver) ----------

/// One `fileReferences/list` candidate.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReferenceCandidate {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub kind: String, // "file" | "directory"
}

/// One `sessionReferenceResolver/candidates` candidate.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReferenceMentionCandidate {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub same_workspace: bool,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub mention: String,
}

// ---------- messageFeedback (CAS ifVersion) ----------

/// `messageFeedback/put` request args.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageFeedbackPutRequest {
    pub session_id: String,
    pub message_id: String,
    pub rating: String, // "positive" | "negative"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<u64>,
}

/// One feedback item (`messageFeedback/list`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageFeedbackItem {
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub rating: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub version: Option<u64>,
}

// ---------- directoryPicker (workspace-controller; optional enhancement) ----------

/// `directoryPicker/list` entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntry {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String, // "file" | "directory"
}

// ---------- SessionJob (session/control jobs mirror) ----------

/// Job status vocabulary (0.1.2-rc.1: no progress field, no stop endpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionJobStatus {
    Running,
    Stopping,
    Completed,
    Killed,
    Failed,
}

/// One `SessionJob` (from control baseline.jobs / `jobs` replacement frames).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionJob {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: Option<SessionJobStatus>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub finished_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newtype_serde_roundtrip() {
        let seq: SessionSeq = serde_json::from_str("42").unwrap();
        assert_eq!(seq, SessionSeq(42));
        assert_eq!(serde_json::to_string(&seq).unwrap(), "42");
        let sid: SessionId = serde_json::from_str("\"s1\"").unwrap();
        assert_eq!(sid.0, "s1");
        assert_eq!(sid.to_string(), "s1");
    }

    #[test]
    fn record_event_parses_with_and_without_request_id() {
        let r: SessionHistoryRecord = serde_json::from_str(
            r#"{"type":"event","event":{"type":"user/message","seq":3,"time":123,"requestId":"req-1","data":{"content":"hi"}}}"#,
        )
        .unwrap();
        match r {
            SessionHistoryRecord::Event { event } => {
                assert_eq!(event.event_type, "user/message");
                assert_eq!(event.seq, Some(SessionSeq(3)));
                assert_eq!(event.request_id.as_deref(), Some("req-1"));
                assert_eq!(event.data.unwrap()["content"], "hi");
            }
            _ => panic!("wrong variant"),
        }
        // A missing requestId is also tolerated (the official server may not
        // send one).
        let r: SessionHistoryRecord = serde_json::from_str(
            r#"{"type":"event","event":{"type":"assistant/message","seq":4,"data":{}}}"#,
        )
        .unwrap();
        match r {
            SessionHistoryRecord::Event { event } => {
                assert_eq!(event.seq, Some(SessionSeq(4)));
                assert_eq!(event.request_id, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn record_chunks_parses_packed_rows() {
        let r: SessionHistoryRecord = serde_json::from_str(
            r#"{"type":"chunks","event":{"type":"chunkrow/text-chunks","texts":["a","b"],"turn":1,"step":2,"index":3,"dt":[1.0]}}"#,
        )
        .unwrap();
        match r {
            SessionHistoryRecord::Chunks {
                event: ChunkRow::TextChunks(d),
            } => {
                assert_eq!(d.texts, vec!["a", "b"]);
                assert_eq!(d.index, Some(3));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn record_unknown_chunkrow_kept_as_unknown() {
        let r: SessionHistoryRecord = serde_json::from_str(
            r#"{"type":"chunks","event":{"type":"chunkrow/future-thing","texts":["x"]}}"#,
        )
        .unwrap();
        match r {
            SessionHistoryRecord::Chunks {
                event: ChunkRow::Unknown { event_type, raw },
            } => {
                assert_eq!(event_type, "chunkrow/future-thing");
                assert_eq!(raw["texts"][0], "x");
            }
            other => panic!("unexpected chunk row: {other:?}"),
        }
    }

    #[test]
    fn list_item_raw_parses_official_wire_shape() {
        // 官方 0.1.2-rc.1 session/list item 形状（live 实证千余条）：顶层
        // sessionId/updatedAt/running/blank/parentSessionId/origin/cwd/
        // projections，title 在 projections.values 内。全部需经逐字段 alias
        // 解析（旧形状 id/updatedAtMs/parentId 与官方不同名）。
        let raw: ListItemRaw = serde_json::from_str(
            r#"{
                "sessionId": "sess-9",
                "updatedAt": 1788864117943,
                "running": false,
                "blank": false,
                "parentSessionId": "parent-1",
                "origin": "subagent",
                "cwd": "/home/nd",
                "projections": {"asOfSeq": 94826, "values": {
                    "title": "部署排查",
                    "sessionListMetadata": {"lastPromptAt": 1788864117943, "blank": false},
                    "turnOutline": [
                        {"turn": 1, "seq": 3, "prompt": "p", "response": "r1"},
                        {"turn": 2, "seq": 6, "prompt": "p2", "response": "r2"}
                    ]
                }}
            }"#,
        )
        .unwrap();
        assert_eq!(raw.id, "sess-9");
        assert_eq!(raw.updated_at_ms, Some(1788864117943));
        assert_eq!(raw.parent_id.as_deref(), Some("parent-1"));
        assert_eq!(raw.cwd.as_deref(), Some("/home/nd"));
        assert_eq!(raw.origin.as_deref(), Some("subagent"));
        assert_eq!(raw.running, Some(false));
        assert_eq!(raw.blank, Some(false));

        let m = meta_from_raw(raw).unwrap();
        assert_eq!(m.id, SessionId("sess-9".into()));
        assert_eq!(m.title.as_deref(), Some("部署排查"));
        assert_eq!(m.cwd.as_deref(), Some("/home/nd"));
        assert_eq!(m.updated_at_ms, 1788864117943);
        assert!(!m.running);
        assert!(!m.blank);
        assert_eq!(m.origin.as_deref(), Some("subagent"));
        assert_eq!(m.parent_id, Some(SessionId("parent-1".into())));
        assert_eq!(m.workspace, None);
        assert_eq!(m.last_turn_preview.as_deref(), Some("r2"));
    }

    #[test]
    fn meta_from_raw_reads_projections_values_and_prefers_updated_at() {
        // 官方形状：title/running/blank/lastPromptAt 等走 projections.values
        // 兜底；顶层 updatedAt（主源）与 values.sessionListMetadata.lastPromptAt
        // 并存时以 updatedAt 为准（live 实证两者同值，浅 item 也只有 updatedAt）。
        let raw: ListItemRaw = serde_json::from_str(
            r#"{
                "sessionId": "sess-7",
                "updatedAt": 2000,
                "projections": {"values": {
                    "title": "部署排查",
                    "cwd": "/home/nd",
                    "running": true,
                    "sessionListMetadata": {"lastPromptAt": 1000, "blank": false},
                    "turnOutline": [
                        {"turn": 1, "seq": 3, "prompt": "p", "response": "r1"},
                        {"turn": 2, "seq": 6, "prompt": "p2", "response": "r2"}
                    ]
                }}
            }"#,
        )
        .unwrap();
        let m = meta_from_raw(raw).unwrap();
        assert_eq!(m.id, SessionId("sess-7".into()));
        assert_eq!(m.title.as_deref(), Some("部署排查"));
        assert_eq!(m.cwd.as_deref(), Some("/home/nd"));
        assert_eq!(m.updated_at_ms, 2000, "顶层 updatedAt 应为 updated_at 主源");
        assert!(m.running);
        assert!(!m.blank);
        assert_eq!(m.workspace, None);
        assert_eq!(m.last_turn_preview.as_deref(), Some("r2"));
    }

    #[test]
    fn meta_from_raw_tolerates_missing_fields_official_shape() {
        // 官方最小 item：仅 sessionId（浅 item 无 projections/origin 等，顶层
        // 字段可整体省略）→ 除 id 外全空但可解析。
        let raw: ListItemRaw = serde_json::from_str(r#"{"sessionId":"sess-8"}"#).unwrap();
        let m = meta_from_raw(raw).unwrap();
        assert_eq!(m.id, SessionId("sess-8".into()));
        assert_eq!(m.title, None);
        assert_eq!(m.cwd, None);
        assert_eq!(m.updated_at_ms, 0);
        assert!(!m.running);
        assert_eq!(m.origin, None);
        assert_eq!(m.parent_id, None);
    }

    #[test]
    fn list_item_raw_still_accepts_legacy_shapes() {
        // 旧 mock/直连形状（snake `id`/`updated_at_ms`/`parent_id` 与顶层
        // workspace）仍兼容：id 是自然主键，updated_at_ms/parent_id 的 snake
        // alias 兜底。
        let raw: ListItemRaw = serde_json::from_str(
            r#"{
                "id": "sess-8",
                "workspace": "ws-1",
                "parent_id": "p8",
                "updated_at_ms": 123
            }"#,
        )
        .unwrap();
        assert_eq!(raw.id, "sess-8");
        assert_eq!(raw.updated_at_ms, Some(123));
        assert_eq!(raw.parent_id.as_deref(), Some("p8"));
        let m = meta_from_raw(raw).unwrap();
        assert_eq!(m.id, SessionId("sess-8".into()));
        assert_eq!(m.updated_at_ms, 123);
        assert_eq!(m.parent_id, Some(SessionId("p8".into())));
        assert_eq!(m.workspace, Some(WorkspaceId("ws-1".into())));
        assert_eq!(m.title, None);
        // camel 自然键（updatedAtMs/parentId）也可用。
        let raw: ListItemRaw =
            serde_json::from_str(r#"{"id":"s2","updatedAtMs":9,"parentId":"q1"}"#).unwrap();
        assert_eq!(raw.updated_at_ms, Some(9));
        assert_eq!(raw.parent_id.as_deref(), Some("q1"));
    }

    #[test]
    fn meta_from_raw_rejects_empty_session_id() {
        // meta_from_raw 依赖非空 id 过滤：官方 sessionId 与旧 id 皆拒绝空串。
        assert!(meta_from_raw(serde_json::from_str(r#"{"sessionId":""}"#).unwrap()).is_none());
        assert!(meta_from_raw(serde_json::from_str(r#"{"id":""}"#).unwrap()).is_none());
    }

    #[test]
    fn follow_frame_snapshot_and_event() {
        let f: FollowFrame = serde_json::from_str(
            r#"{"type":"snapshot","cursor":10,"records":[],"hasMore":false,"projections":{"values":{"title":"t"}}}"#,
        )
        .unwrap();
        match f {
            FollowFrame::Snapshot {
                cursor,
                has_more,
                projections,
                ..
            } => {
                assert_eq!(cursor, Some(SessionLogOffset(10)));
                assert_eq!(has_more, Some(false));
                assert_eq!(projections.unwrap()["values"]["title"], "t");
            }
            _ => panic!("wrong variant"),
        }
        let f: FollowFrame = serde_json::from_str(
            r#"{"type":"event","event":{"type":"turn/end","seq":11,"requestId":"r9"}}"#,
        )
        .unwrap();
        assert!(matches!(f, FollowFrame::Event { event } if event.seq == Some(SessionSeq(11))));
    }
}
