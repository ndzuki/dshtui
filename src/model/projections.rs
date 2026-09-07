//! Projection snapshot (Notes/03 §5, ADR-008).
//!
//! The status bar numbers **all come from the official projections — the TUI
//! never self-computes**. This module is only a thin wrapper for field reads
//! and formatting; missing fields are always tolerated (None), statistics are
//! never invented.

use serde_json::Value;

/// Official projections snapshot (raw preserved as-is; all reads go through
/// getters).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectionSnapshot {
    pub raw: Value,
}

/// Context pressure (contextPressure): pressureTokens/projectedTokens/
/// contextWindow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextPressure {
    pub pressure_tokens: Option<u64>,
    pub projected_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

impl ContextPressure {
    /// Used percentage (pressure/projected); if either is missing → None.
    pub fn percent(&self) -> Option<u64> {
        let p = self.pressure_tokens?;
        let total = self.projected_tokens?;
        if total == 0 {
            return None;
        }
        Some(p.saturating_mul(100) / total)
    }
}

/// token usage (tokenUsage): input/output/cache read/write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

/// Session stats (sessionStats): turns/steps/llmMs/toolMs/ttftMs/ttftSteps/
/// decodeMs/decodeTokens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    pub turns: Option<u64>,
    pub steps: Option<u64>,
    pub llm_ms: Option<u64>,
    pub tool_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub ttft_steps: Option<u64>,
    pub decode_ms: Option<u64>,
    pub decode_tokens: Option<u64>,
}

/// Model selection (modelSelection): lastUsed/next.
///
/// Tolerates BOTH the legacy string shape (`{"lastUsed":"deepseek-chat"}`,
/// Notes/03 §5 alpha.3) and the 0.1.2-rc.1 object shape
/// (`{"lastUsed":{"provider","model","reasoningEffort?"}}`): each is
/// normalized to a display string (`provider/model` for objects, raw for
/// strings). ADR-008 — read-only, never self-computed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelSelection {
    pub last_used: Option<String>,
    pub next: Option<String>,
}

/// Normalize one `modelSelection.lastUsed|next` value to a display string
/// (string passthrough / object `provider/model`). Missing/unknown → None.
fn wire_model_display(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(_) => {
            let provider = value.get("provider").and_then(Value::as_str).unwrap_or("");
            let model = value.get("model").and_then(Value::as_str).unwrap_or("");
            if model.is_empty() && provider.is_empty() {
                None
            } else if provider.is_empty() {
                Some(model.to_string())
            } else if model.is_empty() {
                Some(provider.to_string())
            } else {
                Some(format!("{provider}/{model}"))
            }
        }
        _ => None,
    }
}

impl ProjectionSnapshot {
    pub fn new(raw: Value) -> Self {
        Self { raw }
    }

    fn get_u64(&self, path: &[&str]) -> Option<u64> {
        let mut cur = &self.raw;
        for (i, key) in path.iter().enumerate() {
            cur = cur.get(*key)?;
            if i + 1 < path.len() && !cur.is_object() {
                return None;
            }
        }
        cur.as_u64()
    }

    fn get_str(&self, path: &[&str]) -> Option<String> {
        let mut cur = &self.raw;
        for key in path {
            cur = cur.get(*key)?;
        }
        cur.as_str().filter(|s| !s.is_empty()).map(String::from)
    }

    fn get_bool(&self, path: &[&str]) -> Option<bool> {
        let mut cur = &self.raw;
        for key in path {
            cur = cur.get(*key)?;
        }
        cur.as_bool()
    }

    /// Title (title / sessionListMetadata.title, official convention).
    pub fn title(&self) -> Option<String> {
        self.get_str(&["title"])
            .or_else(|| self.get_str(&["sessionListMetadata", "title"]))
    }

    /// Working directory (cwd).
    pub fn cwd(&self) -> Option<String> {
        self.get_str(&["cwd"])
    }

    /// Running state (official running projection).
    pub fn running(&self) -> Option<bool> {
        self.get_bool(&["running"])
    }

    pub fn context_pressure(&self) -> ContextPressure {
        ContextPressure {
            pressure_tokens: self.get_u64(&["contextPressure", "pressureTokens"]),
            projected_tokens: self.get_u64(&["contextPressure", "projectedTokens"]),
            context_window: self.get_u64(&["contextPressure", "contextWindow"]),
        }
    }

    pub fn token_usage(&self) -> TokenUsage {
        TokenUsage {
            input: self.get_u64(&["tokenUsage", "input"]),
            output: self.get_u64(&["tokenUsage", "output"]),
            cache_read: self.get_u64(&["tokenUsage", "cacheRead"]),
            cache_write: self.get_u64(&["tokenUsage", "cacheWrite"]),
        }
    }

    pub fn session_stats(&self) -> SessionStats {
        SessionStats {
            turns: self.get_u64(&["sessionStats", "turns"]),
            steps: self.get_u64(&["sessionStats", "steps"]),
            llm_ms: self.get_u64(&["sessionStats", "llmMs"]),
            tool_ms: self.get_u64(&["sessionStats", "toolMs"]),
            ttft_ms: self.get_u64(&["sessionStats", "ttftMs"]),
            ttft_steps: self.get_u64(&["sessionStats", "ttftSteps"]),
            decode_ms: self.get_u64(&["sessionStats", "decodeMs"]),
            decode_tokens: self.get_u64(&["sessionStats", "decodeTokens"]),
        }
    }

    pub fn model_selection(&self) -> ModelSelection {
        let raw = self.raw.get("modelSelection");
        ModelSelection {
            last_used: wire_model_display(raw.and_then(|m| m.get("lastUsed"))),
            next: wire_model_display(raw.and_then(|m| m.get("next"))),
        }
    }

    /// Whether `next` differs from `lastUsed` (the status bar shows
    /// `last → next`; AC-006-08 状态条显示与实际一致）。
    pub fn model_selection_changed(&self) -> bool {
        let sel = self.model_selection();
        sel.next.is_some() && sel.next != sel.last_used
    }

    /// contextBreakdown 投影明细（system/tools/message tokens，官方 shape
    /// `[未验证]` 容忍——值可为数字或 `{tokens}` 对象）。只读不自算
    /// （ADR-008），缺失/未知形状返回空。
    pub fn context_breakdown(&self) -> Vec<(&'static str, u64)> {
        let Some(breakdown) = self.raw.get("contextBreakdown") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for key in ["system", "tools", "message"] {
            let Some(v) = breakdown.get(key) else {
                continue;
            };
            let tokens = match v {
                Value::Number(_) => v.as_u64(),
                Value::Object(map) => map
                    .get("tokens")
                    .or_else(|| map.get("token"))
                    .and_then(Value::as_u64),
                _ => None,
            };
            if let Some(n) = tokens {
                out.push((key, n));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ProjectionSnapshot {
        ProjectionSnapshot::new(serde_json::json!({
            "title": "部署排查",
            "cwd": "/home/nd",
            "running": true,
            "contextPressure": {"pressureTokens": 4000, "projectedTokens": 8000, "contextWindow": 128000},
            "tokenUsage": {"input": 100, "output": 250, "cacheRead": 30, "cacheWrite": 5},
            "sessionStats": {"turns": 27, "steps": 1144, "llmMs": 1000, "toolMs": 500,
                             "ttftMs": 200, "ttftSteps": 1, "decodeMs": 800, "decodeTokens": 300},
            "modelSelection": {"lastUsed": "deepseek-chat", "next": "deepseek-chat"}
        }))
    }

    #[test]
    fn reads_official_projection_values() {
        let p = sample();
        assert_eq!(p.title().as_deref(), Some("部署排查"));
        assert_eq!(p.cwd().as_deref(), Some("/home/nd"));
        assert_eq!(p.running(), Some(true));
        let ctx = p.context_pressure();
        assert_eq!(ctx.pressure_tokens, Some(4000));
        assert_eq!(ctx.percent(), Some(50));
        let tok = p.token_usage();
        assert_eq!(tok.output, Some(250));
        assert_eq!(tok.cache_read, Some(30));
        let stats = p.session_stats();
        assert_eq!(stats.turns, Some(27));
        assert_eq!(stats.steps, Some(1144));
        assert_eq!(
            p.model_selection().last_used.as_deref(),
            Some("deepseek-chat")
        );
    }

    #[test]
    fn missing_fields_tolerated() {
        let p = ProjectionSnapshot::new(serde_json::json!({}));
        assert_eq!(p.title(), None);
        assert_eq!(p.running(), None);
        assert_eq!(p.context_pressure().percent(), None);
        assert_eq!(p.token_usage(), TokenUsage::default());
        assert_eq!(p.session_stats(), SessionStats::default());
        assert_eq!(p.model_selection(), ModelSelection::default());
        assert!(!p.model_selection_changed());
    }

    #[test]
    fn model_selection_tolerates_object_and_string_shapes_ac006_08() {
        // 0.1.2-rc.1 对象形状。
        let p = ProjectionSnapshot::new(serde_json::json!({
            "modelSelection": {
                "lastUsed": {"provider": "deepseek_official", "model": "deepseek-chat"},
                "next": {"provider": "deepseek_official", "model": "deepseek-reasoner", "reasoningEffort": "high"}
            }
        }));
        assert_eq!(
            p.model_selection().last_used.as_deref(),
            Some("deepseek_official/deepseek-chat")
        );
        assert_eq!(
            p.model_selection().next.as_deref(),
            Some("deepseek_official/deepseek-reasoner")
        );
        assert!(p.model_selection_changed(), "next ≠ lastUsed");

        // legacy 字符串形状。
        let p = ProjectionSnapshot::new(serde_json::json!({
            "modelSelection": {"lastUsed": "deepseek-chat", "next": "deepseek-chat"}
        }));
        assert_eq!(
            p.model_selection().last_used.as_deref(),
            Some("deepseek-chat")
        );
        assert!(!p.model_selection_changed());
    }

    #[test]
    fn context_percent_never_self_computed_elsewhere() {
        // percent only formats a division, it creates no new statistic; a zero
        // denominator must be None, not panic/NaN.
        let p = ProjectionSnapshot::new(serde_json::json!({
            "contextPressure": {"pressureTokens": 10, "projectedTokens": 0}
        }));
        assert_eq!(p.context_pressure().percent(), None);
        let p = ProjectionSnapshot::new(serde_json::json!({
            "contextPressure": {"pressureTokens": 0, "projectedTokens": 100}
        }));
        assert_eq!(p.context_pressure().percent(), Some(0));
    }

    #[test]
    fn wrong_shape_returns_none_not_panic() {
        let p = ProjectionSnapshot::new(serde_json::json!({
            "contextPressure": "not-an-object",
            "tokenUsage": [1, 2]
        }));
        assert_eq!(p.context_pressure(), ContextPressure::default());
        assert_eq!(p.token_usage(), TokenUsage::default());
    }
}
