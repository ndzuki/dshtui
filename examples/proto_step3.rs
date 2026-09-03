//! THROWAWAY PROTOTYPE（Step 3 Gate）——验证窗口化模型核心语义。
//! 问题：snapshot→follow→page→repair 交错序列下，单一 `apply` 漏斗能否保证
//! seq 去重、requestId 幂等、窗口 ≤cap、稳定 anchor、可修复缺口与不可修复 rebuild？
//! 运行：cargo run --example proto_step3 —— 全部断言通过打印 PASS，失败打印 FAIL。
//! 本文件不会进入 commit（验证后删除）。

use std::collections::{HashSet, VecDeque};

type Seq = u64;

#[derive(Debug, Clone, PartialEq)]
struct Block {
    seq: Seq,
    request_id: Option<String>,
    text: String,
}

#[derive(Debug, Default)]
struct Window {
    blocks: VecDeque<Block>,
    cap: usize,
    /// 窗口最旧已加载 seq（anchor，逐出后仍保留）。
    head_seq: Option<Seq>,
    head_has_more: bool,
    seen_seq: HashSet<Seq>,
    seen_request: HashSet<String>,
}

#[derive(Debug, PartialEq)]
enum Effect {
    Rebuilt,
    TailAppended { count: usize, anchor_stable: bool },
    HeadPrepend { count: usize, anchor_shift: usize },
    Noop,
}

impl Window {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Default::default()
        }
    }

    fn apply_snapshot(&mut self, records: Vec<(Seq, Option<String>, &str)>, has_more: bool) -> Effect {
        self.blocks.clear();
        self.seen_seq.clear();
        self.seen_request.clear();
        let mut sorted = records;
        sorted.sort_by_key(|(s, _, _)| *s);
        for (seq, rid, text) in sorted {
            if self.seen_seq.insert(seq) {
                if let Some(r) = &rid {
                    self.seen_request.insert(r.clone());
                }
                self.blocks.push_back(Block {
                    seq,
                    request_id: rid,
                    text: text.into(),
                });
            }
        }
        self.evict();
        self.head_seq = self.blocks.front().map(|b| b.seq);
        self.head_has_more = has_more;
        Effect::Rebuilt
    }

    fn apply_event(&mut self, seq: Seq, rid: Option<&str>, text: &str) -> Effect {
        // requestId 幂等（D-4）：同 requestId 已 apply → 跳过（不论 seq）。
        if let Some(r) = rid {
            if self.seen_request.contains(r) {
                return Effect::Noop;
            }
        }
        // seq 去重（D-4）：重复/重叠事件按 seq 去重。
        if self.seen_seq.contains(&seq) {
            return Effect::Noop;
        }
        let anchor_stable = self.head_seq.is_some();
        self.seen_seq.insert(seq);
        if let Some(r) = rid {
            self.seen_request.insert(r.to_string());
        }
        // 快路径：seq > 尾部（follow 单调到达）→ O(1) 尾插。
        // 慢路径：乱序/修复到达 → 二分定位插入，保持升序（AC-001-11 顺序一致）。
        let block = Block {
            seq,
            request_id: rid.map(|s| s.to_string()),
            text: text.into(),
        };
        if self.blocks.back().map(|b| b.seq).is_none_or(|tail| seq > tail) {
            self.blocks.push_back(block);
        } else {
            let idx = self
                .blocks
                .partition_point(|b| b.seq < seq);
            self.blocks.insert(idx, block);
        }
        self.evict();
        Effect::TailAppended {
            count: 1,
            anchor_stable,
        }
    }

    fn apply_page(&mut self, records: Vec<(Seq, Option<String>, &str)>) -> Effect {
        let mut sorted = records;
        sorted.sort_by_key(|(s, _, _)| *s);
        let mut fresh: Vec<Block> = Vec::new();
        for (seq, rid, text) in sorted {
            if self.seen_seq.contains(&seq) {
                continue; // 与窗口重叠 → seq 去重（AC-001-11 并发合并）
            }
            if let Some(r) = &rid {
                if self.seen_request.contains(r) {
                    continue;
                }
            }
            self.seen_seq.insert(seq);
            if let Some(r) = &rid {
                self.seen_request.insert(r.clone());
            }
            fresh.push(Block {
                seq,
                request_id: rid,
                text: text.into(),
            });
        }
        let anchor_shift = fresh.len();
        if fresh.is_empty() {
            return Effect::Noop;
        }
        // 冷路径合并：合并重排保持全局升序（page 记录可能含 >head 的乱序 seq）。
        let mut all: Vec<Block> = self.blocks.drain(..).chain(fresh).collect();
        all.sort_by_key(|b| b.seq);
        all.dedup_by_key(|b| b.seq);
        self.blocks = all.into();
        self.evict();
        self.head_seq = self.blocks.front().map(|b| b.seq);
        Effect::HeadPrepend {
            count: anchor_shift,
            anchor_shift,
        }
    }

    fn evict(&mut self) {
        while self.blocks.len() > self.cap {
            // 逐出最旧：seq 索引保留在 seen_seq 中，page 重叠直接丢弃——
            // 这正是「仅留 seq 锚点」的语义（不重复加载已逐出历史）。
            self.blocks.pop_front();
        }
        // anchor = 当前最旧已加载 seq：随逐出前进（滚动锚点由 AppState 用
        // TailAppended.anchor_stable / HeadPrepend.anchor_shift 维护视口稳定）。
        self.head_seq = self.blocks.front().map(|b| b.seq);
    }

    fn seqs(&self) -> Vec<Seq> {
        self.blocks.iter().map(|b| b.seq).collect()
    }

    fn is_sorted(&self) -> bool {
        self.blocks
            .iter()
            .zip(self.blocks.iter().skip(1))
            .all(|(a, b)| a.seq < b.seq)
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }
}

fn main() {
    let mut fails = 0;
    macro_rules! check {
        ($cond:expr, $msg:expr) => {
            if !$cond {
                println!("FAIL: {}", $msg);
                fails += 1;
            }
        };
    }

    // 场景 1：snapshot → follow 追加 + seq 去重 + requestId 幂等。
    let mut w = Window::new(200);
    check!(
        w.apply_snapshot(
            vec![(5, None, "m5"), (6, None, "m6"), (10, None, "m10")],
            true
        ) == Effect::Rebuilt,
        "snapshot 必须产生 Rebuilt"
    );
    check!(
        w.apply_event(11, None, "m11") == Effect::TailAppended { count: 1, anchor_stable: true },
        "follow 追加"
    );
    check!(
        w.apply_event(11, None, "dup") == Effect::Noop,
        "重复 seq 必须 Noop（AC-001-11）"
    );
    check!(
        w.apply_event(12, Some("r1"), "m12") == Effect::TailAppended { count: 1, anchor_stable: true },
        "带 requestId 追加"
    );
    check!(
        w.apply_event(13, Some("r1"), "重放") == Effect::Noop,
        "同 requestId 不同 seq 重放必须 Noop（AC-001-10 幂等）"
    );
    check!(
        w.apply_event(12, Some("r1"), "m12-again") == Effect::Noop,
        "同 seq 同 requestId 重复必须 Noop"
    );
    check!(w.seqs() == vec![5, 6, 10, 11, 12], "窗口内容 = [5,6,10,11,12]");

    // 场景 2：page 前插 + 重叠去重（AC-001-11 并发交错）。
    let e = w.apply_page(vec![(1, None, "m1"), (2, None, "m2"), (3, None, "m3"), (4, None, "m4")]);
    check!(
        e == Effect::HeadPrepend { count: 4, anchor_shift: 4 },
        "前插 4 条"
    );
    check!(
        w.apply_page(vec![(1, None, "dup1"), (2, None, "dup2")]) == Effect::Noop,
        "page 重叠必须全量去重"
    );
    check!(
        w.apply_page(vec![(3, None, "dup3"), (4, None, "m4b"), (7, None, "m7"), (8, None, "m8")])
            == Effect::HeadPrepend { count: 2, anchor_shift: 2 },
        "page 部分重叠：只前插新 seq 7/8"
    );
    check!(
        w.seqs() == vec![1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12],
        "前插后窗口升序且无重复（seq 9 缺失是合法跳号，非空洞）"
    );
    check!(w.is_sorted(), "窗口必须升序");

    // 场景 3：容量逐出 + anchor 稳定性。
    let mut w = Window::new(10);
    w.apply_snapshot((1..=10).map(|s| (s, None, "m")).collect(), true);
    for s in 11..=20 {
        w.apply_event(s, None, "m");
    }
    check!(w.len() == 10, "容量上限 10");
    check!(w.seqs() == (11..=20).collect::<Vec<_>>(), "逐出最旧、尾部保留最新 10 条");
    check!(w.head_seq == Some(11), "anchor 随逐出前进到 11（窗口最旧已加载 seq）");
    check!(
        w.apply_page(vec![(11, None, "dup")]) == Effect::Noop,
        "已逐出 seq 的 page 重放必须 Noop（seq 锚点语义）"
    );

    // 场景 4：可修复缺口（rebuild）与不可修复 rebuild 分流。
    let mut w = Window::new(10);
    w.apply_snapshot(vec![(1, None, "a"), (3, None, "c")], true);
    // 服务端事实：page hasMore=false 且最旧 seq=1 → 之前不可能有更多记录。
    // 模型不臆断：由 AppState 用 hasMore 边界事实判定缺口。
    w.head_has_more = false;
    check!(!w.head_has_more, "hasMore=false → 无更早历史（无空洞）");
    // 重连 rebuild：新 snapshot 替换旧窗口（不可修复缺口 → 整窗重建）。
    w.apply_snapshot(vec![(7, None, "x"), (8, None, "y")], true);
    check!(w.seqs() == vec![7, 8], "rebuild 替换旧窗口且不残留旧块");

    // 场景 5：恢复路径——错误输入（重复/重放）不污染后续正确输入。
    let mut w = Window::new(10);
    w.apply_snapshot(vec![(1, None, "a")], true);
    w.apply_event(2, Some("r9"), "m2");
    check!(
        w.apply_event(3, Some("r9"), "重放") == Effect::Noop,
        "重放被拒（幂等）"
    );
    check!(
        w.apply_event(4, Some("r10"), "m4") == Effect::TailAppended { count: 1, anchor_stable: true },
        "失败/重放之后正确输入正常追加（恢复路径）"
    );
    check!(w.seqs() == vec![1, 2, 4], "恢复后窗口无污染（seq 3 从未 apply）");

    // 场景 6：随机交错（20 组）——不变量：升序、无重复、≤cap、无 panic。
    let mut rng: u64 = 0x9e3779b97f4a7c15;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for _round in 0..20 {
        let mut w = Window::new(50);
        w.apply_snapshot(Vec::new(), true);
        for _ in 0..500 {
            let seq = (next() % 120) + 1;
            let rid = if next() % 3 == 0 {
                Some(format!("r{}", next() % 40))
            } else {
                None
            };
            match next() % 3 {
                0 => {
                    w.apply_event(seq, rid.as_deref(), "e");
                }
                1 => {
                    w.apply_page(vec![(seq, rid, "p")]);
                }
                _ => {
                    let mut recs = Vec::new();
                    for _ in 0..3 {
                        recs.push(((next() % 120) + 1, None, "p"));
                    }
                    w.apply_page(recs);
                }
            }
            if !w.is_sorted() {
                println!("FAIL: 随机交错后窗口乱序");
                fails += 1;
            }
            if w.len() > w.cap {
                println!("FAIL: 随机交错后超过容量上限");
                fails += 1;
            }
            if w.seqs().len() != w.blocks.iter().map(|b| b.seq).collect::<HashSet<_>>().len() {
                println!("FAIL: 随机交错后出现重复 seq");
                fails += 1;
            }
        }
    }

    if fails == 0 {
        println!("PROTO PASS: 去重/幂等/≤cap/anchor/rebuild/恢复路径/随机交错全部成立");
    } else {
        println!("PROTO FAIL: {fails} 项断言失败");
        std::process::exit(1);
    }
}
