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

/// Raw `session/list` entry (compatible with both shapes: direct fields /
/// projections.values, Notes/03 §5).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListItemRaw {
    pub id: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub projections: Option<serde_json::Value>,
    // Direct-field fallback (still readable when the official shape changes).
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
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
    let updated_at_ms = metadata
        .and_then(|m| m.get("lastPromptAt"))
        .and_then(|x| x.as_i64())
        .or(raw.updated_at_ms)
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
    fn meta_from_raw_reads_projections_values_shape() {
        let raw: ListItemRaw = serde_json::from_str(
            r#"{
                "id": "sess-7",
                "workspace": "ws-1",
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
        assert_eq!(m.updated_at_ms, 1000);
        assert!(m.running);
        assert!(!m.blank);
        assert_eq!(m.workspace, Some(WorkspaceId("ws-1".into())));
        assert_eq!(m.last_turn_preview.as_deref(), Some("r2"));
    }

    #[test]
    fn meta_from_raw_tolerates_missing_fields() {
        let raw: ListItemRaw = serde_json::from_str(r#"{"id":"sess-8"}"#).unwrap();
        let m = meta_from_raw(raw).unwrap();
        assert_eq!(m.id, SessionId("sess-8".into()));
        assert_eq!(m.title, None);
        assert_eq!(m.updated_at_ms, 0);
        assert!(!m.running);
    }

    #[test]
    fn meta_from_raw_rejects_empty_id() {
        let raw: ListItemRaw = serde_json::from_str(r#"{"id":""}"#).unwrap();
        assert!(meta_from_raw(raw).is_none());
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
