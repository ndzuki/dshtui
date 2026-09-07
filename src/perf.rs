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

    /// p50 帧间隔（ms；样本不足返回 0）。
    pub fn p50(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut s = self.samples.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
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

/// 可选 perf 日志：仅调用方显式开启时写一行（REQ §6 不刷屏）。
pub fn log_perf_line(path: &str, rss_mb: f64, frame_p50_ms: f64, poll_ms: f64) {
    let line = format!("rss_mb={rss_mb:.1} frame_p50_ms={frame_p50_ms:.1} poll_ms={poll_ms:.1}\n");
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
}
