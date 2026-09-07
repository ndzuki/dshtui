//! KB 预检索统计模型（REQ-009 §5）：`/kb-stats` 嵌套 hist → `KbStatsSnapshot`。
//!
//! - 桶边界 = agent-server 常量 `KB_DURATION_BOUNDARIES`（AC-009-07 口径）；
//! - `boundaries` 与 `counts` 等长断言（不等长 = 契约漂移，fail-fast 保留
//!   上一份快照，REQ-009 §6 轮询失败不清空已显示数据）。

use crate::api::monitor::WireKbStats;

/// agent-server `KB_DURATION_BOUNDARIES`（agent-server.mjs L205 实读）。
pub const KB_DURATION_BOUNDARIES: [i64; 7] = [0, 100, 500, 1000, 2000, 4000, 16000];

/// 单段统计桶（totals/window 同形）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KbBucket {
    pub hits: i64,
    pub misses: i64,
    pub empty: i64,
    pub errs: i64,
    pub skipped: i64,
    pub searches: i64,
    pub avg_ms: i64,
    pub hist: KbHistogram,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KbHistogram {
    pub boundaries: Vec<i64>,
    pub counts: Vec<i64>,
}

impl KbHistogram {
    /// 命中率相关辅助：桶边界与计数等长（契约断言）。
    pub fn is_valid(&self) -> bool {
        !self.boundaries.is_empty() && self.boundaries.len() == self.counts.len()
    }
}

/// `/kb-stats` 快照（累计 + 当前小时两段）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KbStatsSnapshot {
    pub totals: KbBucket,
    pub window: KbBucket,
    pub last_log_at_ms: i64,
    pub restored: bool,
}

impl KbStatsSnapshot {
    /// wire → 内部模型；边界/counts 不等长 → 契约漂移错误（fail-fast，
    /// 上层保留旧快照）。
    pub fn from_wire(w: &WireKbStats) -> Result<Self, KbStatsError> {
        let totals = bucket_from_wire(&w.totals)?;
        let window = bucket_from_wire(&w.window)?;
        Ok(Self {
            totals,
            window,
            last_log_at_ms: w.last_log_at,
            restored: w.restored,
        })
    }

    /// 命中率（命中 / (命中+未命中)，无检索时 0；展示口径对齐
    /// `agent-monitor.html renderKBStats`）。
    pub fn hit_ratio(&self) -> u32 {
        let total = self.totals.hits + self.totals.misses;
        if total > 0 {
            (self.totals.hits * 100 / total) as u32
        } else {
            0
        }
    }

    /// 桶边界是否与 agent-server 常量一致（AC-009-07 口径断言，展示前
    /// 校验；漂移时 UI 用返回的 boundaries 兜底呈现）。
    pub fn boundaries_match_agent_server(&self) -> bool {
        self.totals.hist.boundaries == KB_DURATION_BOUNDARIES.to_vec()
            && self.window.hist.boundaries == KB_DURATION_BOUNDARIES.to_vec()
    }
}

fn bucket_from_wire(w: &crate::api::monitor::WireKbBucket) -> Result<KbBucket, KbStatsError> {
    let hist = KbHistogram {
        boundaries: w.hist.boundaries.clone(),
        counts: w.hist.counts.clone(),
    };
    if !hist.is_valid() {
        return Err(KbStatsError::HistogramMismatch {
            boundaries: hist.boundaries.len(),
            counts: hist.counts.len(),
        });
    }
    Ok(KbBucket {
        hits: w.hits,
        misses: w.misses,
        empty: w.empty,
        errs: w.errs,
        skipped: w.skipped,
        searches: w.searches,
        avg_ms: w.avg_ms,
        hist,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum KbStatsError {
    #[error("KB 直方图契约漂移: boundaries 长度 {boundaries} != counts 长度 {counts}")]
    HistogramMismatch { boundaries: usize, counts: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::monitor::WireKbBucket;

    fn wire() -> WireKbStats {
        WireKbStats {
            totals: WireKbBucket {
                hits: 10,
                misses: 2,
                empty: 1,
                errs: 0,
                skipped: 1,
                searches: 12,
                avg_ms: 345,
                hist: crate::api::monitor::WireHistogram {
                    boundaries: KB_DURATION_BOUNDARIES.to_vec(),
                    counts: vec![3, 2, 1, 4, 1, 1, 0],
                },
            },
            window: WireKbBucket {
                hits: 1,
                misses: 0,
                empty: 0,
                errs: 0,
                skipped: 0,
                searches: 1,
                avg_ms: 120,
                hist: crate::api::monitor::WireHistogram {
                    boundaries: KB_DURATION_BOUNDARIES.to_vec(),
                    counts: vec![0, 0, 0, 0, 1, 0, 0],
                },
            },
            last_log_at: 1788515073345,
            restored: true,
        }
    }

    #[test]
    fn from_wire_nested_hist_and_ratio() {
        let s = KbStatsSnapshot::from_wire(&wire()).unwrap();
        assert_eq!(s.totals.hits, 10);
        assert_eq!(s.totals.avg_ms, 345);
        assert_eq!(s.hit_ratio(), 83); // 10/12
        assert!(s.boundaries_match_agent_server());
        assert!(s.restored);
    }

    #[test]
    fn histogram_mismatch_fails_fast() {
        let mut w = wire();
        w.totals.hist.counts.pop();
        let err = KbStatsSnapshot::from_wire(&w).unwrap_err();
        assert!(
            matches!(
                err,
                KbStatsError::HistogramMismatch {
                    boundaries: 7,
                    counts: 6
                }
            ),
            "err={err}"
        );
    }

    #[test]
    fn zero_searches_ratio_is_zero() {
        let mut w = wire();
        w.totals.hits = 0;
        w.totals.misses = 0;
        let s = KbStatsSnapshot::from_wire(&w).unwrap();
        assert_eq!(s.hit_ratio(), 0);
    }

    #[test]
    fn boundaries_constant_matches_agent_server() {
        // 桶边界 = agent-server 常量（AC-009-07 口径）。
        assert_eq!(
            KB_DURATION_BOUNDARIES,
            [0, 100, 500, 1000, 2000, 4000, 16000]
        );
    }
}
