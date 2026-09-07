//! Agent roster 模型（REQ-009 §5）：`/agents` wire → `AgentRosterEntry`、
//! `RosterSnapshot`（幂等合并）、`stageKey()` STAGE 映射与展示辅助。
//!
//! - 同 sessionId 重复 poll 幂等更新（原地覆盖，无重复条目闪烁，AC-009-10）；
//! - `stageKey()` 口径与 `agent-monitor.html:634` 逐字一致；
//! - 状态条/roster/详情展示数字全部来自 agent-server 数据（ADR-008），本层
//!   不自算。

use std::collections::HashMap;

use crate::api::monitor::WireAgent;
use crate::api::types::SessionId;

/// agent 工作状态（wire `status` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
}

impl AgentStatus {
    pub fn from_wire(v: &str) -> Self {
        if v == "idle" {
            AgentStatus::Idle
        } else {
            AgentStatus::Working
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
        }
    }
}

/// 会话类型（wire `kind` 字段；未知值保守归为 session）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Subagent,
    Task,
    Session,
}

impl AgentKind {
    pub fn from_wire(v: &str) -> Self {
        match v {
            "subagent" => AgentKind::Subagent,
            "task" => AgentKind::Task,
            _ => AgentKind::Session,
        }
    }
}

/// `/agents` 条目 → 内部模型（REQ-009 §5 字段级类型表；命名 snake_case）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRosterEntry {
    pub session_id: SessionId,
    pub phase: String,
    /// 展示标签（wire task 多行取首行，由 UI 层消费时截断）。
    pub task: String,
    pub project: String,
    pub task_id: String,
    pub status: AgentStatus,
    /// 任务 frontmatter 状态（`stageKey()` 输入；可为空）。
    pub task_status: String,
    pub elapsed_sec: u64,
    pub last_event_at_ms: i64,
    pub seq: i64,
    pub label: String,
    pub kind: AgentKind,
    pub parent_session_id: Option<String>,
    pub delegation_depth: u32,
    pub provider: String,
    pub model: String,
}

impl AgentRosterEntry {
    pub fn from_wire(w: &WireAgent) -> Self {
        Self {
            session_id: SessionId::new(w.session_id.clone()),
            phase: w.phase.clone(),
            task: w.task.clone(),
            project: w.project.clone(),
            task_id: w.task_id.clone(),
            status: AgentStatus::from_wire(&w.status),
            task_status: w.task_status.clone(),
            elapsed_sec: w.elapsed.max(0) as u64,
            last_event_at_ms: w.last_event_at,
            seq: w.seq,
            label: w.label.clone(),
            kind: AgentKind::from_wire(&w.kind),
            parent_session_id: if w.parent_session_id.is_empty() {
                None
            } else {
                Some(w.parent_session_id.clone())
            },
            delegation_depth: w.delegation_depth.max(0) as u32,
            provider: w.provider.clone(),
            model: w.model.clone(),
        }
    }

    /// `agentName()`（agent-monitor.html:646）：label 优先 → task 首行截断 →
    /// 短 sessionId。
    pub fn display_name(&self) -> String {
        let label = self.label.trim();
        if !label.is_empty() {
            return label.to_string();
        }
        let task = self.task.split('\n').next().unwrap_or("").trim();
        if !task.is_empty() {
            return if task.chars().count() > 26 {
                task.chars().take(26).collect::<String>() + "…"
            } else {
                task.to_string()
            };
        }
        short_session(&self.session_id.0)
    }

    /// 任务标题首行（chat 首问 kbQuery / 详情归属任务用）。
    pub fn task_first_line(&self) -> String {
        self.task
            .split('\n')
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// `stageKey()` 映射后的阶段键（STAGE 全集之一，含 idle/working）。
    pub fn stage_key(&self) -> &'static str {
        stage_key(self.task_status.as_str(), self.phase.as_str(), self.status)
    }
}

/// roster 快照：有序条目（插入序）+ 最近完成计数 + 连接状态（REQ-009 §5）。
#[derive(Debug, Clone, Default)]
pub struct RosterSnapshot {
    entries: HashMap<SessionId, AgentRosterEntry>,
    order: Vec<SessionId>,
    pub finished: u32,
    pub connected: bool,
    pub last_poll_at_ms: i64,
}

impl RosterSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// 幂等合并一轮 `/agents`：同 sessionId 原地覆盖（保持首次插入位置，
    /// 无重复条目闪烁，AC-009-10）；消失的条目移除。返回（新增, 更新, 移除）
    /// 计数供日志/状态提示。
    pub fn merge(&mut self, wire: &[WireAgent], finished: u32) -> (usize, usize, usize) {
        let mut added = 0;
        let mut updated = 0;
        let mut seen: Vec<SessionId> = Vec::with_capacity(wire.len());
        for w in wire {
            if w.session_id.is_empty() {
                continue;
            }
            let entry = AgentRosterEntry::from_wire(w);
            let sid = entry.session_id.clone();
            seen.push(sid.clone());
            match self.entries.get(&sid) {
                None => {
                    self.order.push(sid.clone());
                    self.entries.insert(sid, entry);
                    added += 1;
                }
                Some(prev) if prev != &entry => {
                    self.entries.insert(sid, entry);
                    updated += 1;
                }
                _ => {}
            }
        }
        let seen_set: std::collections::HashSet<_> = seen.iter().cloned().collect();
        let mut removed = 0;
        self.order.retain(|sid| {
            if seen_set.contains(sid) {
                true
            } else {
                self.entries.remove(sid);
                removed += 1;
                false
            }
        });
        self.finished = finished;
        (added, updated, removed)
    }

    /// 有序快照（插入序；展示排序由 UI 按 stageKey 处理）。
    pub fn ordered(&self) -> Vec<&AgentRosterEntry> {
        self.order
            .iter()
            .filter_map(|sid| self.entries.get(sid))
            .collect()
    }

    pub fn get(&self, sid: &SessionId) -> Option<&AgentRosterEntry> {
        self.entries.get(sid)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------- STAGE 映射（agent-monitor.html STAGE/stageKey 同口径） ----------

/// STAGE 元数据（label/occupation/building/zone/color/emoji/symbol/action/bubble）。
#[derive(Debug, Clone, Copy)]
pub struct StageMeta {
    pub key: &'static str,
    pub label: &'static str,
    pub occupation: &'static str,
    pub building: &'static str,
    pub zone: &'static str,
    pub color: &'static str,
    pub emoji: &'static str,
    pub symbol: &'static str,
    pub action: &'static str,
    pub bubble: &'static str,
}

/// STAGE 全集（21 stage，agent-monitor.html L202-224 逐字）。
pub static STAGES: &[StageMeta] = &[
    StageMeta {
        key: "refining",
        label: "需求精炼",
        occupation: "需求精炼师",
        building: "需求精炼坊",
        zone: "北文职区",
        color: "#a78bfa",
        emoji: "📖",
        symbol: "📚",
        action: "研读需求",
        bubble: "提炼需求…",
    },
    StageMeta {
        key: "planning",
        label: "方案规划",
        occupation: "规划设计师",
        building: "规划院",
        zone: "北文职区",
        color: "#38bdf8",
        emoji: "🧭",
        symbol: "🗺️",
        action: "绘制路线",
        bubble: "推演方案…",
    },
    StageMeta {
        key: "plan-review",
        label: "计划评审",
        occupation: "评审官",
        building: "评审厅",
        zone: "北文职区",
        color: "#facc15",
        emoji: "⚖️",
        symbol: "⚖️",
        action: "核对计划",
        bubble: "查漏洞…",
    },
    StageMeta {
        key: "implementing",
        label: "实现建造",
        occupation: "开发工程师",
        building: "开发工厂",
        zone: "东工业区",
        color: "#3b82f6",
        emoji: "🏭",
        symbol: "🔨",
        action: "敲码建造",
        bubble: "动手实现…",
    },
    StageMeta {
        key: "review",
        label: "验收检查",
        occupation: "验收工程师",
        building: "验收场",
        zone: "东工业区",
        color: "#4ade80",
        emoji: "🎯",
        symbol: "🛡️",
        action: "逐条验收",
        bubble: "核对 AC…",
    },
    StageMeta {
        key: "merge",
        label: "合并交付",
        occupation: "发布工程师",
        building: "合并发布站",
        zone: "东工业区",
        color: "#06b6d4",
        emoji: "🛰️",
        symbol: "🔀",
        action: "合并发布",
        bubble: "打包发布…",
    },
    StageMeta {
        key: "audit",
        label: "独立审计",
        occupation: "审计师",
        building: "审计塔",
        zone: "西知识区",
        color: "#fb923c",
        emoji: "🔍",
        symbol: "📜",
        action: "独立复核",
        bubble: "独立复核…",
    },
    StageMeta {
        key: "priority",
        label: "优先级",
        occupation: "优先级评估师",
        building: "优先级塔",
        zone: "北文职区",
        color: "#f87171",
        emoji: "🚩",
        symbol: "🚩",
        action: "排定优先级",
        bubble: "该先做…",
    },
    StageMeta {
        key: "pm",
        label: "统筹问答",
        occupation: "统筹 PM",
        building: "统筹议事厅",
        zone: "中央广场",
        color: "#f472b6",
        emoji: "🏛️",
        symbol: "🗣️",
        action: "访谈统筹",
        bubble: "聊聊决策…",
    },
    StageMeta {
        key: "design",
        label: "设计蓝图",
        occupation: "架构设计师",
        building: "设计工坊",
        zone: "北文职区",
        color: "#c084fc",
        emoji: "🎨",
        symbol: "📐",
        action: "绘制蓝图",
        bubble: "画架构…",
    },
    StageMeta {
        key: "split",
        label: "任务拆分",
        occupation: "任务拆分师",
        building: "拆分工作台",
        zone: "东北区",
        color: "#14b8a6",
        emoji: "🧩",
        symbol: "🧩",
        action: "拆分任务",
        bubble: "拆小块…",
    },
    StageMeta {
        key: "conflict",
        label: "冲突处理",
        occupation: "仲裁员",
        building: "仲裁庭",
        zone: "东工业区",
        color: "#f43f5e",
        emoji: "⚔️",
        symbol: "🛠️",
        action: "解决冲突",
        bubble: "要仲裁…",
    },
    StageMeta {
        key: "blocked",
        label: "阻塞等待",
        occupation: "阻塞等待者",
        building: "阻塞等待亭",
        zone: "中央广场",
        color: "#ef4444",
        emoji: "🚧",
        symbol: "⛔",
        action: "等待解除",
        bubble: "卡住了…",
    },
    StageMeta {
        key: "needs-grilling",
        label: "待访谈",
        occupation: "待访谈者",
        building: "访谈茶馆",
        zone: "南自然区",
        color: "#facc15",
        emoji: "💬",
        symbol: "❓",
        action: "等待访谈",
        bubble: "不清楚…",
    },
    StageMeta {
        key: "done",
        label: "已完成",
        occupation: "交付庆祝者",
        building: "凯旋广场",
        zone: "中央广场",
        color: "#84cc16",
        emoji: "🏆",
        symbol: "✅",
        action: "交付庆祝",
        bubble: "完成！",
    },
    StageMeta {
        key: "closed",
        label: "已关闭",
        occupation: "归档管理员",
        building: "归档小馆",
        zone: "西知识区",
        color: "#64748b",
        emoji: "🗄️",
        symbol: "🔒",
        action: "归档封存",
        bubble: "已归档…",
    },
    StageMeta {
        key: "ready",
        label: "待命出发",
        occupation: "待命者",
        building: "待命驿站",
        zone: "中央广场",
        color: "#94a3b8",
        emoji: "⏳",
        symbol: "🕐",
        action: "等待派发",
        bubble: "随时出发…",
    },
    StageMeta {
        key: "conventions",
        label: "规范审查",
        occupation: "规范审查员",
        building: "规范文书房",
        zone: "北文职区",
        color: "#34d399",
        emoji: "📏",
        symbol: "📏",
        action: "检查规范",
        bubble: "对规范…",
    },
    StageMeta {
        key: "knowledge",
        label: "知识归档",
        occupation: "知识管理员",
        building: "知识树馆",
        zone: "西知识区",
        color: "#60a5fa",
        emoji: "🌳",
        symbol: "📚",
        action: "提炼知识",
        bubble: "沉淀知识…",
    },
    StageMeta {
        key: "idle",
        label: "休息",
        occupation: "休息居民",
        building: "休息草坪",
        zone: "南自然区",
        color: "#64748b",
        emoji: "🍃",
        symbol: "☕",
        action: "恢复体力",
        bubble: "休息中…",
    },
    StageMeta {
        key: "working",
        label: "工作中",
        occupation: "综合工作者",
        building: "综合工位",
        zone: "中央广场",
        color: "#94a3b8",
        emoji: "⚙️",
        symbol: "⚙️",
        action: "执行中",
        bubble: "忙碌中…",
    },
];

/// STAGE 查找（未知键回退 working，与 `agent-monitor.html` `STAGE[a.key] ||
/// STAGE.working` 一致）。
pub fn stage_meta(key: &str) -> &'static StageMeta {
    STAGES.iter().find(|s| s.key == key).unwrap_or(&STAGES[20])
}

/// `stageKey()`（agent-monitor.html:634 逐字口径）：
/// taskStatus → phase 各自经 map 归一后匹配 STAGE 全集，否则按 status 兜底。
pub fn stage_key(task_status: &str, phase: &str, status: AgentStatus) -> &'static str {
    // map 归一（round1→planning、round2→implementing、plan_review→plan-review）
    // + STAGE 全集匹配；返回 'static（map 目标与 STAGE key 均为静态字面量）。
    fn mapped(v: &str) -> Option<&'static str> {
        match v {
            "round1" => Some("planning"),
            "round2" => Some("implementing"),
            "plan_review" => Some("plan-review"),
            _ => STAGES.iter().find(|s| s.key == v).map(|s| s.key),
        }
    }
    let ts = task_status.trim();
    if !ts.is_empty() {
        if let Some(k) = mapped(ts) {
            return k;
        }
    }
    let ph = phase.trim();
    if !ph.is_empty() {
        if let Some(k) = mapped(ph) {
            return k;
        }
    }
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
    }
}

/// `shortSession()`（agent-monitor.html:642）。
pub fn short_session(sid: &str) -> String {
    if sid.is_empty() {
        return "agent".to_string();
    }
    if let Some(rest) = sid.strip_prefix("session-") {
        return rest.chars().take(6).collect();
    }
    sid.chars().take(6).collect()
}

/// `fmtElapsed()`（agent-monitor.html:643）：h/m/s 紧凑格式。
pub fn fmt_elapsed(sec: u64) -> String {
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// `esc()`（agent-monitor.html:644）：HTML 转义（安全边界 REQ-009 §7：
/// agent 文本仅作展示，渲染前转义，不解释为控制序列）。
pub fn esc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(sid: &str, task_status: &str, phase: &str, status: &str) -> WireAgent {
        WireAgent {
            session_id: sid.into(),
            phase: phase.into(),
            task: "任务A\n第二行".into(),
            project: "proj".into(),
            task_id: "TASK-1".into(),
            status: status.into(),
            task_status: task_status.into(),
            elapsed: 3661,
            last_event_at: 1000,
            seq: 5,
            label: "".into(),
            kind: "subagent".into(),
            parent_session_id: "".into(),
            delegation_depth: 1,
            provider: "p".into(),
            model: "m".into(),
        }
    }

    #[test]
    fn from_wire_maps_all_fields() {
        let e = AgentRosterEntry::from_wire(&wire("session-x", "implementing", "x", "working"));
        assert_eq!(e.session_id.0, "session-x");
        assert_eq!(e.status, AgentStatus::Working);
        assert_eq!(e.kind, AgentKind::Subagent);
        assert_eq!(e.elapsed_sec, 3661);
        assert_eq!(e.delegation_depth, 1);
        assert_eq!(e.parent_session_id, None);
        assert_eq!(e.task_first_line(), "任务A");
    }

    #[test]
    fn negative_elapsed_clamped_to_zero() {
        let mut w = wire("s", "", "", "idle");
        w.elapsed = -5;
        let e = AgentRosterEntry::from_wire(&w);
        assert_eq!(e.elapsed_sec, 0);
    }

    #[test]
    fn display_name_precedence_label_task_short_session() {
        let mut w = wire("session-abc123def", "", "", "idle");
        w.label = "子代理A".into();
        assert_eq!(AgentRosterEntry::from_wire(&w).display_name(), "子代理A");

        w.label = "".into();
        assert_eq!(AgentRosterEntry::from_wire(&w).display_name(), "任务A");

        w.task = "".into();
        assert_eq!(AgentRosterEntry::from_wire(&w).display_name(), "abc123");
    }

    #[test]
    fn stage_key_full_mapping_matrix() {
        // taskStatus 优先；map 归一；phase 兜底；status 兜底。
        assert_eq!(
            stage_key("implementing", "x", AgentStatus::Working),
            "implementing"
        );
        assert_eq!(stage_key("round1", "x", AgentStatus::Working), "planning");
        assert_eq!(
            stage_key("round2", "x", AgentStatus::Working),
            "implementing"
        );
        assert_eq!(
            stage_key("plan_review", "x", AgentStatus::Working),
            "plan-review"
        );
        assert_eq!(
            stage_key("unknown-stage", "audit", AgentStatus::Working),
            "audit"
        );
        assert_eq!(
            stage_key("", "needs-grilling", AgentStatus::Working),
            "needs-grilling"
        );
        assert_eq!(stage_key("", "", AgentStatus::Idle), "idle");
        assert_eq!(stage_key("", "", AgentStatus::Working), "working");
        assert_eq!(stage_key("", "unknown", AgentStatus::Working), "working");
        assert_eq!(stage_key("", "", AgentStatus::Working), "working");
        // STAGE 全集 21 key 均存在且可映射（agent-monitor.html 同口径）。
        assert_eq!(STAGES.len(), 21);
        for s in STAGES {
            assert_eq!(stage_key(s.key, "", AgentStatus::Working), s.key);
        }
    }

    #[test]
    fn roster_merge_is_idempotent_and_removes_gone_agents() {
        let mut snap = RosterSnapshot::new();
        let (add, upd, rem) = snap.merge(
            &[
                wire("s1", "implementing", "", "working"),
                wire("s2", "", "", "idle"),
            ],
            2,
        );
        assert_eq!((add, upd, rem), (2, 0, 0));
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.finished, 2);
        let order_before = snap.ordered().len();

        // 同数据重复 poll：幂等（无新增/更新，AC-009-10 无重复闪烁）。
        let (add, upd, rem) = snap.merge(
            &[
                wire("s1", "implementing", "", "working"),
                wire("s2", "", "", "idle"),
            ],
            2,
        );
        assert_eq!((add, upd, rem), (0, 0, 0));
        assert_eq!(snap.ordered().len(), order_before);

        // 字段变更：原地覆盖，插入位置不变（首个元素仍是 s1）。
        let mut w1 = wire("s1", "review", "", "working");
        w1.seq = 9;
        let (add, upd, rem) = snap.merge(&[w1, wire("s2", "", "", "idle")], 3);
        assert_eq!((add, upd, rem), (0, 1, 0));
        assert_eq!(snap.ordered()[0].session_id.0, "s1");
        assert_eq!(snap.ordered()[0].seq, 9);
        assert_eq!(snap.finished, 3);

        // 消失的条目移除（恢复路径：删除后再出现 → 重新添加）。
        let (add, _upd, rem) = snap.merge(&[wire("s1", "review", "", "working")], 0);
        assert_eq!((add, rem), (0, 1));
        assert_eq!(snap.len(), 1);
        let (add, _upd, _rem) = snap.merge(
            &[
                wire("s1", "review", "", "working"),
                wire("s2", "", "", "idle"),
            ],
            0,
        );
        assert_eq!(add, 1);
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn merge_skips_empty_session_ids() {
        let mut snap = RosterSnapshot::new();
        let mut w = wire("", "", "", "idle");
        w.session_id = "".into();
        let (add, _, _) = snap.merge(&[w], 0);
        assert_eq!(add, 0);
        assert!(snap.is_empty());
    }

    #[test]
    fn fmt_elapsed_formats() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(61), "1m1s");
        assert_eq!(fmt_elapsed(3661), "1h1m");
    }

    #[test]
    fn short_session_and_esc() {
        assert_eq!(short_session(""), "agent");
        assert_eq!(short_session("session-abcdefgh"), "abcdef");
        assert_eq!(short_session("xyz123456789"), "xyz123");
        assert_eq!(esc("<a>&\"'"), "&lt;a&gt;&amp;&quot;&#39;");
        assert_eq!(esc("普通文本"), "普通文本");
    }

    #[test]
    fn stage_meta_falls_back_to_working() {
        assert_eq!(stage_meta("nope").key, "working");
        assert_eq!(stage_meta("implementing").building, "开发工厂");
    }
}
