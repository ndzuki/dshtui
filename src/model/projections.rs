//! 投影快照（Notes/03 §5，ADR-008）。
//!
//! 状态条数字**全部读取官方 projections，TUI 不自算**——本模块只做字段
//! 读取与格式话的薄封装；字段缺失一律容忍（None），绝不臆造统计。

use serde_json::Value;

/// 官方 projections 快照（raw 原样保留；读取全部走 getter）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectionSnapshot {
    pub raw: Value,
}

/// 上下文压力（contextPressure）：pressureTokens/projectedTokens/contextWindow。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextPressure {
    pub pressure_tokens: Option<u64>,
    pub projected_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

impl ContextPressure {
    /// 已用百分比（pressure/projected）；两者任一缺失 → None。
    pub fn percent(&self) -> Option<u64> {
        let p = self.pressure_tokens?;
        let total = self.projected_tokens?;
        if total == 0 {
            return None;
        }
        Some(p.saturating_mul(100) / total)
    }
}

/// token 用量（tokenUsage）：input/output/cache read/write。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

/// 会话统计（sessionStats）：turns/steps/llmMs/toolMs/ttftMs/ttftSteps/decodeMs/decodeTokens。
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

/// 模型选择（modelSelection）：lastUsed/next。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelSelection {
    pub last_used: Option<String>,
    pub next: Option<String>,
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

    /// 标题（title / sessionListMetadata.title，官方口径）。
    pub fn title(&self) -> Option<String> {
        self.get_str(&["title"])
            .or_else(|| self.get_str(&["sessionListMetadata", "title"]))
    }

    /// 工作目录（cwd）。
    pub fn cwd(&self) -> Option<String> {
        self.get_str(&["cwd"])
    }

    /// 运行中状态（官方 running 投影）。
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
        ModelSelection {
            last_used: self.get_str(&["modelSelection", "lastUsed"]),
            next: self.get_str(&["modelSelection", "next"]),
        }
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
    }

    #[test]
    fn context_percent_never_self_computed_elsewhere() {
        // percent 只做除法格式化，不产生新统计；分母 0 必须 None 而非 panic/NaN。
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
