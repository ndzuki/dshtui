//! 性能测量（REQ-009 §5 内存/性能预算 + Notes/06 §8 口径）：
//! RSS 读取（`/proc/self/status`）、`frame_ms` p50 采样、poll 延迟记录。
//!
//! - 采样器无全局状态（纯函数 + 显式状态），测试友好；
//! - perf 日志为可选诊断输出（`/tmp/dshtui-perf.log`，仅显式开启）。

use std::time::Instant;

/// 读当前进程 RSS（KiB → MB，Linux `/proc/self/status`；非 Linux 回退 0）。
pub fn rss_mb() -> f64 {
    match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb: u64 = rest
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    return kb as f64 / 1024.0;
                }
            }
            0.0
        }
        Err(_) => 0.0,
    }
}

/// 帧时长 p50 采样器（滑动窗口）。
#[derive(Debug, Default)]
pub struct FrameSampler {
    samples: Vec<f64>,
    last: Option<Instant>,
}

impl FrameSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记一帧开始（返回距上一帧的间隔 ms，首帧返回 0）。
    pub fn tick(&mut self) -> f64 {
        let now = Instant::now();
        let elapsed_ms = match self.last {
            Some(prev) => now.duration_since(prev).as_secs_f64() * 1000.0,
            None => 0.0,
        };
        self.last = Some(now);
        if elapsed_ms > 0.0 {
            self.samples.push(elapsed_ms);
            if self.samples.len() > 600 {
                self.samples.remove(0);
            }
        }
        elapsed_ms
    }

    /// p 分位帧间隔（ms；样本不足返回 0）。`p` ∈ (0,1]，0.5=p50、0.99=p99。
    /// 线性插值口径与 Notes/06 §8 一致（sort 后取 `p*(n-1)` 处）。
    pub fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut s = self.samples.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if p <= 0.0 {
            return s[0];
        }
        if p >= 1.0 {
            return s[s.len() - 1];
        }
        let pos = p * (s.len() - 1) as f64;
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        if lo == hi {
            return s[lo];
        }
        let frac = pos - lo as f64;
        s[lo] + (s[hi] - s[lo]) * frac
    }

    /// p50 帧间隔（ms；样本不足返回 0）。
    pub fn p50(&self) -> f64 {
        self.percentile(0.5)
    }

    /// p99 帧间隔（ms；样本不足返回 0）。
    pub fn p99(&self) -> f64 {
        self.percentile(0.99)
    }

    pub fn count(&self) -> usize {
        self.samples.len()
    }
}

/// poll 延迟记录（最近一次 /agents 往返 ms 与时间戳）。
#[derive(Debug, Clone, Copy, Default)]
pub struct PollLatency {
    pub last_ms: f64,
    pub last_at_ms: i64,
}

/// Perf 日志行（REQ-008 §5 `PerfLogEntry` 字段表：ts/rss_kb/frame_ms_p50/
/// frame_ms_p99/search_ms/page_latency_ms/ws_reconnects；Notes/06 §8 口径）。
/// 可复用 `FrameSampler` 之外的上层上报值（search/page/reconnect 计数）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PerfLogEntry {
    /// 采样时间戳（毫秒，UNIX epoch 口径）。
    pub ts_ms: i64,
    /// 进程 RSS（KB；`rss_mb()` × 1024 或 `/proc` 原生 KB）。
    pub rss_kb: u64,
    pub frame_ms_p50: f64,
    pub frame_ms_p99: f64,
    /// 检索耗时 ms（最近一次 session/search + 本地 nucleo query）。
    pub search_ms: Option<f64>,
    /// 分页/翻页耗时 ms（最近一次 session/page）。
    pub page_latency_ms: Option<f64>,
    /// WS 重连累计次数（AppState reducer 单调计数）。
    pub ws_reconnects: u64,
}

impl PerfLogEntry {
    /// 单行 key=val 文本（兼容既有 `/tmp/dshtui-perf.log` 行形态，追加写入）。
    pub fn to_line(&self) -> String {
        let mut line = format!(
            "ts={} rss_kb={} frame_ms_p50={:.1} frame_ms_p99={:.1}",
            self.ts_ms, self.rss_kb, self.frame_ms_p50, self.frame_ms_p99
        );
        if let Some(v) = self.search_ms {
            line.push_str(&format!(" search_ms={v:.1}"));
        }
        if let Some(v) = self.page_latency_ms {
            line.push_str(&format!(" page_latency_ms={v:.1}"));
        }
        line.push_str(&format!(" ws_reconnects={}\n", self.ws_reconnects));
        line
    }
}

/// 追加一行 REQ-008 perf 日志（path 为空 = 禁用；写失败静默，不刷屏）。
pub fn log_perf_entry(path: &str, entry: &PerfLogEntry) {
    if path.is_empty() {
        return;
    }
    append_line(path, &entry.to_line());
}

/// 可选 perf 日志：仅调用方显式开启时写一行（REQ §6 不刷屏）。
/// REQ-009 monitor 专用形态（poll_ms 为 agent-server 轮询口径，非 REQ-008
/// 字段）；主 TUI 用 `log_perf_entry`。
pub fn log_perf_line(path: &str, rss_mb: f64, frame_p50_ms: f64, poll_ms: f64) {
    let line = format!("rss_mb={rss_mb:.1} frame_p50_ms={frame_p50_ms:.1} poll_ms={poll_ms:.1}\n");
    append_line(path, &line);
}

fn append_line(path: &str, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// glibc 堆 free-list 归还（Linux only；其它平台/非 glibc 静默 no-op）。
/// kitty 帧编码每帧 churn 大缓冲（payload/deflate/b64），glibc 会把这些
/// 内存留在 arena 抬高 RSS——周期性 trim 保持 <20MB（AC-009-09）。
#[cfg(target_os = "linux")]
pub fn malloc_trim() {
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn malloc_trim() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_reading_is_positive_on_linux() {
        let mb = rss_mb();
        assert!(mb > 0.0, "RSS 应可读取（Linux /proc）: {mb}");
        assert!(mb < 4096.0, "RSS 异常: {mb}");
    }

    #[test]
    fn frame_sampler_computes_p50() {
        let mut s = FrameSampler::new();
        assert_eq!(s.tick(), 0.0, "首帧无间隔");
        // 用 sleep 制造已知间隔（宽松断言：p50 介于样本范围）。
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.tick();
        std::thread::sleep(std::time::Duration::from_millis(10));
        s.tick();
        std::thread::sleep(std::time::Duration::from_millis(10));
        s.tick();
        assert_eq!(s.count(), 3);
        let p50 = s.p50();
        assert!((5.0..=25.0).contains(&p50), "p50 应在样本范围内: {p50}");
    }

    #[test]
    fn poll_latency_defaults_to_zero() {
        let p = PollLatency::default();
        assert_eq!(p.last_ms, 0.0);
        assert_eq!(p.last_at_ms, 0);
    }

    // ---------- REQ-008 Step 1：percentile / p99 ----------

    #[test]
    fn percentile_is_exact_for_fixed_samples() {
        // 确定样本（非时钟依赖）：1..=10 的 p50=5.5、p99≈9.91、p0=1、p100=10。
        let s = FrameSampler {
            samples: (1..=10).map(f64::from).collect(),
            last: None,
        };
        assert!((s.percentile(0.0) - 1.0).abs() < 1e-9);
        assert!((s.percentile(1.0) - 10.0).abs() < 1e-9);
        assert!((s.percentile(0.5) - 5.5).abs() < 1e-9, "p50 线性插值");
        // 线性插值：pos=0.99*9=8.91 → s[8]+0.91*(s[9]-s[8]) = 9+0.91 = 9.91。
        assert!((s.percentile(0.99) - 9.91).abs() < 1e-9, "p99 线性插值");
        assert!((s.p99() - 9.91).abs() < 1e-9);
        // p50 与既有实现同值（回归）。
        assert!((s.p50() - 5.5).abs() < 1e-9);
    }

    #[test]
    fn percentile_empty_or_single_sample() {
        let empty = FrameSampler::default();
        assert_eq!(empty.percentile(0.5), 0.0);
        assert_eq!(empty.p99(), 0.0);
        let single = FrameSampler {
            samples: vec![3.0],
            last: None,
        };
        assert_eq!(single.p50(), 3.0);
        assert_eq!(single.p99(), 3.0);
    }

    #[test]
    fn frame_sampler_p99_derived_from_p50_ok() {
        // p99 需要 ≥1 样本即可；p99 >= p50 恒成立（同分布排序）。
        let mut s = FrameSampler::new();
        for v in [5.0f64, 5.0, 20.0, 20.0, 20.0, 20.0] {
            // 直接注入样本（samples 私有但测试在同模块）。
            s.samples.push(v);
        }
        let p50 = s.p50();
        let p99 = s.p99();
        assert!(p99 >= p50, "p99({p99}) 应 ≥ p50({p50})");
        assert!(p99 > 5.0, "尾部高值应抬高 p99: {p99}");
    }

    #[test]
    fn perf_log_entry_line_has_all_req008_fields() {
        let e = PerfLogEntry {
            ts_ms: 1_700_000_000_000,
            rss_kb: 18_000,
            frame_ms_p50: 33.0,
            frame_ms_p99: 40.0,
            search_ms: Some(12.3),
            page_latency_ms: Some(200.0),
            ws_reconnects: 2,
        };
        let line = e.to_line();
        assert!(
            line.starts_with("ts=1700000000000 rss_kb=18000 frame_ms_p50=33.0 frame_ms_p99=40.0")
        );
        assert!(line.contains(" search_ms=12.3"), "line={line}");
        assert!(line.contains(" page_latency_ms=200.0"), "line={line}");
        assert!(line.contains(" ws_reconnects=2"), "line={line}");
        assert!(line.ends_with('\n'));
        // Option 未填时不写该 key（兼容既有行形态、不刷空值）。
        let e2 = PerfLogEntry::default();
        let l2 = e2.to_line();
        assert!(!l2.contains("search_ms="), "l2={l2}");
        assert!(!l2.contains("page_latency_ms="), "l2={l2}");
        assert!(l2.contains(" rss_kb=0"));
    }

    #[test]
    fn log_perf_entry_empty_path_is_noop_and_roundtrips() {
        // 空 path = 禁用：不创建文件。
        log_perf_entry("", &PerfLogEntry::default());
        let dir = std::env::temp_dir().join(format!("dshtui-perf-entry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perf.log");
        let p = path.to_string_lossy().into_owned();
        log_perf_entry(
            &p,
            &PerfLogEntry {
                ts_ms: 1,
                rss_kb: 2,
                frame_ms_p50: 1.5,
                frame_ms_p99: 2.5,
                search_ms: Some(3.0),
                page_latency_ms: None,
                ws_reconnects: 0,
            },
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("ts=1 rss_kb=2 frame_ms_p50=1.5 frame_ms_p99=2.5"));
        assert!(content.contains("search_ms=3.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
