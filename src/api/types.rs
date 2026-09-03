//! 强类型 newtype 与 wire 结构（Notes/03 §4/§5/§7 兼容动作 #1）。
//!
//! 兼容策略（Notes/03 §7）：未知字段一律容忍（`#[serde(default)]`），未知事件
//! 类型原样跳过但计入日志。字段命名以官方 Remote API 实读为准（D-3=A）。

use serde::{Deserialize, Serialize};

// ---------- newtype（对齐官方 SessionSeq/SessionLogOffset 强类型方向） ----------

macro_rules! newtype {
    ($name:ident, $inner:ty, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

newtype!(SessionId, String, "会话标识（官方 session id，字符串）");
newtype!(WorkspaceId, String, "工作区/项目标识");
newtype!(SessionSeq, u64, "事件序列号（单调）");
newtype!(SessionLogOffset, u64, "日志 offset（page/follow 游标语义）");
newtype!(RequestId, String, "请求幂等键（D-4：重复送达按 requestId 幂等 apply）");

// 数值 newtype 允许 Copy（窗口 reducer 高频拷贝，避免 move 噪声）。
impl Copy for SessionSeq {}
impl Copy for SessionLogOffset {}

// 字符串 newtype 提供 Default（容器/derive 需要；空值仅在反序列化占位使用）。
impl Default for SessionId {
    fn default() -> Self {
        Self(String::new())
    }
}
impl Default for WorkspaceId {
    fn default() -> Self {
        Self(String::new())
    }
}

// ---------- session/page / session/follow 地址 ----------

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

// ---------- SessionHistoryRecord（Notes/03 §4.3 两类） ----------

/// `{type:"event", event:{...}}` 或 `{type:"chunks", event:{chunkrow}}`。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionHistoryRecord {
    #[serde(rename = "event")]
    Event { event: SessionWireEvent },
    #[serde(rename = "chunks")]
    Chunks { event: ChunkRow },
}

/// 事件记录：type/seq/time/data + requestId（D-4 幂等键）+ 可忽略标记等。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWireEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub seq: Option<SessionSeq>,
    #[serde(default)]
    pub time: Option<i64>,
    /// 幂等键：同 requestId 重复送达只 apply 一次（REQ-002 AC-002-06 基础契约）。
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub ignorable: Option<bool>,
    #[serde(default)]
    pub source_event_seqs: Option<Vec<u64>>,
    #[serde(default)]
    pub surface_op: Option<String>,
    /// 原始 payload：未知块原样保留（D-4），不解析不丢弃。
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// packed chunk row（原样存储，不展开为逐 delta——官方低内存优化的关键）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChunkRow {
    #[serde(rename = "chunkrow/text-chunks")]
    TextChunks(ChunkData),
    #[serde(rename = "chunkrow/reasoning-chunks")]
    ReasoningChunks(ChunkData),
    #[serde(rename = "chunkrow/tool-call-chunks")]
    ToolCallChunks(ToolCallChunkData),
    /// 未知 chunkrow 类型：原样保留 event type + payload（D-4 下游契约）。
    #[serde(other)]
    Unknown,
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

// ---------- session/follow 帧（Notes/03 §4.2） ----------

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum FollowFrame {
    /// 注意：serde 内部 tag 枚举的 variant 字段不受枚举级 rename_all 影响，
    /// 必须逐字段显式 rename（camelCase 协议形状）。
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

// ---------- session/page 响应（Notes/03 §4.1） ----------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PageResult {
    #[serde(default)]
    pub records: Vec<SessionHistoryRecord>,
    #[serde(default)]
    pub has_more: Option<bool>,
}

// ---------- 会话列表（session/list 轻量元数据，Notes/06 §1） ----------

/// TUI 只保留轻量结构（~500B/条）；其余 projections 丢弃。
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

/// session/list 原始条目（兼容两种形态：直接字段 / projections.values，Notes/03 §5）。
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
    // 直接字段兜底（官方形态变化时仍可读）。
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

/// 从原始条目提取轻量元数据（容忍字段缺失；id 缺失视为无效行）。
pub fn meta_from_raw(raw: ListItemRaw) -> Option<SessionMeta> {
    if raw.id.is_empty() {
        return None;
    }
    // projections 形态：{"values": {title, ...}}；sessionListMetadata 提供 lastPromptAt/blank。
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
        keys.iter().find_map(|k| vals.and_then(|v| v.get(k)).and_then(|x| x.as_bool()))
    };
    let metadata = vals.and_then(|v| v.get("sessionListMetadata"));
    let updated_at_ms = metadata
        .and_then(|m| m.get("lastPromptAt"))
        .and_then(|x| x.as_i64())
        .or(raw.updated_at_ms)
        .unwrap_or(0);

    // turnOutline 末条 response 摘要（≤120 字符，官方截断口径）。
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
            .or_else(|| metadata.and_then(|m| m.get("blank")).and_then(|x| x.as_bool()))
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
        // 无 requestId 也容忍（官方可能不携带）。
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
        assert!(matches!(
            r,
            SessionHistoryRecord::Chunks {
                event: ChunkRow::Unknown
            }
        ));
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
