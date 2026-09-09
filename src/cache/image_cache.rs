//! REQ-004 V0.2 图片缓存（Step 3）：LRU + 预算记账 + 单飞 + 并发安全。
//!
//! 口径（REQ-004 §4/§5，D-15；Notes/06 §6/§9）：
//! - 键 `attachment_id`（FR-004-01 事实修正：内部/缓存键统一 snake_case）；
//! - 预算总账 = Σ(`bytes` + `temp_file` 占用) ≤ `cache_bytes`（默认 32MB，
//!   config 已有）；超预算 LRU 驱逐最旧；`cache_bytes=0` 不缓存（拉取即弃）；
//! - 单飞：`in_flight` 表保证同 `attachment_id` 仅一次 fetch+decode
//!   （AC-004-08/09）；
//! - 并发安全：内部 `Mutex` 短临界区（单写多读精神，02 §3）——fetch/decode
//!   在 `spawn_blocking` 外完成，缓存只做账本与文件托管，不与渲染帧竞态；
//! - 临时文件落系统临时目录（随机名），进程退出（Drop）清理，不落盘会话
//!   内容（06 §9）。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use lru::LruCache;

use crate::api::types::{AttachmentId, MediaType};
use crate::model::ImageCacheEntry;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct CacheInner {
    entries: LruCache<AttachmentId, ImageCacheEntry>,
    /// 每条目的预算记账（bytes + 临时文件占用；文件可能已被外部删除，
    /// 内部表保持精确）。
    sizes: HashMap<AttachmentId, u64>,
    in_flight: HashSet<AttachmentId>,
    pinned: HashSet<AttachmentId>,
    used_bytes: u64,
    budget_bytes: u64,
    /// 单调时间戳（last_used 分配源）。
    clock: u64,
}

/// `acquire` 的结果：唯一执行者 / 已在途 / 命中缓存。
#[derive(Debug, PartialEq, Eq)]
pub enum Acquire {
    /// 本调用者是唯一执行者——必须完成 fetch+decode 后 `complete` 或
    /// 失败后 `abort`。
    Started,
    /// 同 attachment_id 已有在途请求（幂等 no-op，AC-004-08）。
    InFlight,
    /// 缓存命中：LRU 触达，直接复用。
    Cached(ImageCacheEntry),
}

/// `complete` 的结果：是否入缓存（超预算/预算 0 → 拉取即弃，调用方删文件）。
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Cached,
    NotCached,
}

/// 进程内图片缓存（Mutex 短临界区，并发安全）。
#[derive(Debug)]
pub struct ImageCache {
    inner: Mutex<CacheInner>,
    temp_dir: PathBuf,
}

impl ImageCache {
    /// 预算 `budget_bytes`（= config `cache_bytes`；0 = 不缓存）。
    pub fn new(budget_bytes: u64) -> Self {
        // 每实例独立临时目录：测试并行 + 多实例互不干扰（Drop 只删自己）。
        let temp_dir = std::env::temp_dir().join(format!(
            "dshtui-img-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&temp_dir);
        Self {
            inner: Mutex::new(CacheInner {
                entries: LruCache::unbounded(),
                sizes: HashMap::new(),
                in_flight: HashSet::new(),
                pinned: HashSet::new(),
                used_bytes: 0,
                budget_bytes,
                clock: 0,
            }),
            temp_dir,
        }
    }

    /// 临时目录（随机名文件所在；测试与清理用）。
    pub fn temp_dir(&self) -> PathBuf {
        self.temp_dir.clone()
    }

    /// Pin an entry while ImageView or an external viewer may still use it.
    pub fn pin(&self, id: &AttachmentId) {
        self.inner
            .lock()
            .expect("image cache lock")
            .pinned
            .insert(id.clone());
    }

    /// Release a previous pin so later budget changes may evict the entry.
    pub fn unpin(&self, id: &AttachmentId) {
        let mut inner = self.inner.lock().expect("image cache lock");
        inner.pinned.remove(id);
        evict_to_budget(&mut inner);
    }

    /// 当前预算。
    pub fn budget(&self) -> u64 {
        self.inner.lock().expect("image cache lock").budget_bytes
    }

    /// 更新预算（config `cache_bytes` 注入；调 0 = 不缓存）。收紧预算立即
    /// 触发 LRU 驱逐至预算内（AC-004-04 口径）。
    pub fn set_budget(&self, budget_bytes: u64) {
        let mut inner = self.inner.lock().expect("image cache lock");
        inner.budget_bytes = budget_bytes;
        evict_to_budget(&mut inner);
    }

    /// 当前记账（Σ bytes + 临时文件占用）。
    pub fn used(&self) -> u64 {
        self.inner.lock().expect("image cache lock").used_bytes
    }

    /// 命中则返回条目副本并触达 LRU。
    pub fn get(&self, id: &AttachmentId) -> Option<ImageCacheEntry> {
        let mut inner = self.inner.lock().expect("image cache lock");
        inner.clock += 1;
        let clock = inner.clock;
        inner.entries.get_mut(id).map(|e| {
            e.last_used = clock;
            e.clone()
        })
    }

    /// 单飞获取入口：缓存命中 → Cached；已有在途 → InFlight；否则登记
    /// 在途并返回 Started。
    pub fn acquire(&self, id: &AttachmentId) -> Acquire {
        let mut inner = self.inner.lock().expect("image cache lock");
        inner.clock += 1;
        let clock = inner.clock;
        if let Some(e) = inner.entries.get_mut(id) {
            e.last_used = clock;
            return Acquire::Cached(e.clone());
        }
        if inner.in_flight.contains(id) {
            return Acquire::InFlight;
        }
        inner.in_flight.insert(id.clone());
        Acquire::Started
    }

    /// 拉取/解码失败：清在途（幂等——失败后修正输入重跑不被旧状态污染，
    /// 恢复路径要求）。
    pub fn abort(&self, id: &AttachmentId) {
        self.inner
            .lock()
            .expect("image cache lock")
            .in_flight
            .remove(id);
    }

    /// 拉取/解码成功：清在途 + 入缓存（预算驱逐）。
    pub fn complete(&self, id: &AttachmentId, entry: ImageCacheEntry) -> InsertOutcome {
        let mut inner = self.inner.lock().expect("image cache lock");
        inner.in_flight.remove(id);
        // A pinned entry is currently displayed or handed to a viewer. Keep its
        // file stable instead of replacing and deleting the file in use.
        if inner.pinned.contains(id) && inner.entries.peek(id).is_some() {
            if entry.temp_file != inner.entries.peek(id).expect("entry exists").temp_file {
                let _ = std::fs::remove_file(&entry.temp_file);
            }
            return InsertOutcome::Cached;
        }
        inner.clock += 1;
        let size = self.entry_size(&entry);
        let mut entry = entry;
        entry.last_used = inner.clock;
        if inner.budget_bytes == 0 || size > inner.budget_bytes {
            return InsertOutcome::NotCached;
        }
        // 同名替换（重取同一 attachment_id）：先清旧账再入新账。
        if let Some(prev) = inner.entries.pop(id) {
            if let Some(prev_size) = inner.sizes.remove(id) {
                inner.used_bytes = inner.used_bytes.saturating_sub(prev_size);
            }
            let _ = std::fs::remove_file(&prev.temp_file);
        }
        inner.entries.put(id.clone(), entry);
        inner.sizes.insert(id.clone(), size);
        inner.used_bytes += size;
        // 超预算 → LRU 驱逐最旧，直至预算内（AC-004-04）。
        evict_to_budget(&mut inner);
        InsertOutcome::Cached
    }

    /// 写入临时文件（随机名 + mediaType 对应扩展名），返回路径。
    pub fn write_temp_file(&self, media_type: &MediaType, bytes: Vec<u8>) -> io::Result<PathBuf> {
        let ext = extension_for(media_type);
        let name = format!(
            "img-{}-{}-{}.{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            ext
        );
        let path = self.temp_dir.join(name);
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    /// 条目预算记账 = 编码字节数 + 临时文件实际占用（文件不可读时回退
    /// `entry.bytes`）。
    fn entry_size(&self, entry: &ImageCacheEntry) -> u64 {
        let file_len = std::fs::metadata(&entry.temp_file)
            .map(|m| m.len())
            .unwrap_or(entry.bytes);
        entry.bytes.saturating_add(file_len)
    }
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        // 进程退出清理：删除临时目录（尽力而为，不落盘会话内容，06 §9）。
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn evict_to_budget(inner: &mut CacheInner) {
    while inner.used_bytes > inner.budget_bytes {
        let Some((evicted_id, evicted)) = pop_evictable(inner) else {
            break;
        };
        let evicted_size = inner.sizes.remove(&evicted_id).unwrap_or(0);
        inner.used_bytes = inner.used_bytes.saturating_sub(evicted_size);
        if let Err(e) = std::fs::remove_file(&evicted.temp_file) {
            tracing::debug!(path = %evicted.temp_file.display(), error = %e, "LRU eviction failed; ignoring");
        }
    }
}

fn pop_evictable(inner: &mut CacheInner) -> Option<(AttachmentId, ImageCacheEntry)> {
    let mut skipped = Vec::new();
    let result = loop {
        let Some(candidate) = inner.entries.pop_lru() else {
            for (id, entry) in skipped {
                inner.entries.push(id, entry);
            }
            return None;
        };
        if inner.pinned.contains(&candidate.0) {
            skipped.push(candidate);
            continue;
        }
        break Some(candidate);
    };
    for (id, entry) in skipped {
        inner.entries.push(id, entry);
    }
    result
}

fn extension_for(media_type: &MediaType) -> &'static str {
    match media_type.get().as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{AttachmentId, MediaType};
    use crate::model::ImageCacheEntry;
    use std::path::Path;
    use std::sync::Arc;
    use std::thread;

    fn entry(id: &str, bytes: u64, dir: &Path) -> ImageCacheEntry {
        let path = dir.join(format!("{id}.png"));
        std::fs::write(&path, vec![0u8; bytes as usize]).unwrap();
        ImageCacheEntry {
            attachment_id: AttachmentId::new(id.into()),
            media_type: MediaType::new("image/png".into()),
            bytes,
            width: 1,
            height: 1,
            temp_file: path,
            last_used: 0,
        }
    }

    #[test]
    fn insert_get_and_lru_touch() {
        let cache = ImageCache::new(1_000);
        let dir = cache.temp_dir();
        assert_eq!(
            cache.complete(&AttachmentId::new("a".into()), entry("a", 100, &dir)),
            InsertOutcome::Cached
        );
        assert_eq!(
            cache.complete(&AttachmentId::new("b".into()), entry("b", 100, &dir)),
            InsertOutcome::Cached
        );
        let got = cache.get(&AttachmentId::new("a".into())).expect("cached");
        assert_eq!(got.attachment_id.get(), "a");
        assert_eq!(got.media_type.get(), "image/png");
    }

    #[test]
    fn budget_evicts_oldest_until_within_budget_and_deletes_temp_files() {
        // 每条目账本 = bytes + 文件占用 = 200；预算 450 只容得下两条。
        let cache = ImageCache::new(450);
        let dir = cache.temp_dir();
        cache.complete(&AttachmentId::new("old".into()), entry("old", 100, &dir));
        cache.complete(&AttachmentId::new("mid".into()), entry("mid", 100, &dir));
        cache.complete(&AttachmentId::new("new".into()), entry("new", 100, &dir));
        // 总账 3×200=600 > 450 → 驱逐最旧直到预算内。
        assert!(cache.used() <= 450, "used={}", cache.used());
        assert!(
            cache.get(&AttachmentId::new("old".into())).is_none(),
            "最旧已驱逐"
        );
        assert!(!dir.join("old.png").exists(), "驱逐必须删除临时文件");
        // 后续 get(new) 触达后，mid 成为最旧。
        assert!(cache.get(&AttachmentId::new("new".into())).is_some());
        assert!(cache.get(&AttachmentId::new("mid".into())).is_some());
        assert!(cache.get(&AttachmentId::new("old".into())).is_none());
    }

    #[test]
    fn pinned_entry_survives_eviction_and_replacement_keeps_original_file() {
        // AC-004-04/05：pin（ImageView/查看器使用中）条目跳过 LRU 驱逐；
        // 同 id 再 complete 时不得替换/删除使用中的文件。
        let cache = ImageCache::new(450);
        let dir = cache.temp_dir();
        cache.complete(&AttachmentId::new("old".into()), entry("old", 100, &dir));
        cache.pin(&AttachmentId::new("old".into()));
        cache.complete(&AttachmentId::new("mid".into()), entry("mid", 100, &dir));
        cache.complete(&AttachmentId::new("new".into()), entry("new", 100, &dir));
        // 总账 3×200=600 > 450：逐出候选跳过 pinned old → 逐出 mid，总账 400。
        assert_eq!(cache.used(), 400, "used={}", cache.used());
        assert!(
            cache.get(&AttachmentId::new("old".into())).is_some(),
            "pinned 条目不得被驱逐"
        );
        assert!(dir.join("old.png").exists(), "pinned 文件保留");
        assert!(
            cache.get(&AttachmentId::new("mid".into())).is_none(),
            "最旧未 pin 条目被驱逐"
        );
        assert!(!dir.join("mid.png").exists(), "被驱逐文件已删除");
        assert!(cache.get(&AttachmentId::new("new".into())).is_some());

        // 同 id 替换（重取）时 pin 仍在 → 保留原条目与原文件，丢弃新文件。
        let replacement = dir.join("old-v2.png");
        std::fs::write(&replacement, vec![0u8; 100]).unwrap();
        let mut e2 = entry("old", 100, &dir);
        e2.temp_file = replacement.clone();
        cache.complete(&AttachmentId::new("old".into()), e2);
        let got = cache.get(&AttachmentId::new("old".into())).unwrap();
        assert_eq!(got.temp_file, dir.join("old.png"), "pin 中不替换条目");
        assert!(!replacement.exists(), "替换文件被丢弃");

        // unpin 后收紧预算 → 恢复可驱逐。
        cache.unpin(&AttachmentId::new("old".into()));
        cache.set_budget(200);
        assert!(cache.used() <= 200, "used={}", cache.used());
    }

    #[test]
    fn oversized_entry_is_not_cached_so_caller_discards() {
        let cache = ImageCache::new(100);
        let dir = cache.temp_dir();
        assert_eq!(
            cache.complete(&AttachmentId::new("big".into()), entry("big", 300, &dir)),
            InsertOutcome::NotCached
        );
        assert_eq!(cache.used(), 0);
        assert!(cache.get(&AttachmentId::new("big".into())).is_none());
        // 拉取即弃：调用方负责删除临时文件。
        std::fs::remove_file(dir.join("big.png")).unwrap();
    }

    #[test]
    fn budget_zero_never_caches() {
        let cache = ImageCache::new(0);
        let dir = cache.temp_dir();
        assert_eq!(
            cache.complete(&AttachmentId::new("a".into()), entry("a", 10, &dir)),
            InsertOutcome::NotCached
        );
        assert!(cache.get(&AttachmentId::new("a".into())).is_none());
        std::fs::remove_file(dir.join("a.png")).unwrap();
    }

    #[test]
    fn acquire_single_flight_dedupes_concurrent_fetchers() {
        // AC-004-09：同 attachment_id 并发 acquire 仅一个 Started（单飞），
        // 其余 InFlight；complete 后全部 Cached；abort 后可重试（恢复路径）。
        let cache = Arc::new(ImageCache::new(10_000));
        let id = Arc::new(AttachmentId::new("same".into()));
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let id = id.clone();
            let started = started.clone();
            let inflight = inflight.clone();
            handles.push(thread::spawn(move || match cache.acquire(&id) {
                Acquire::Started => {
                    started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                Acquire::InFlight => {
                    inflight.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                Acquire::Cached(_) => panic!("首轮不应命中缓存"),
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1, "单飞");
        assert_eq!(
            inflight.load(std::sync::atomic::Ordering::SeqCst),
            7,
            "其余并发请求判在途"
        );

        // 失败 → abort 清在途（失败后修正重跑不被旧失败状态污染）。
        cache.abort(&id);
        assert_eq!(cache.acquire(&id), Acquire::Started);

        // 成功 → complete 落地缓存，后续 acquire 直接命中。
        let dir = cache.temp_dir();
        cache.complete(&id, entry("same", 100, &dir));
        match cache.acquire(&id) {
            Acquire::Cached(e) => assert_eq!(e.attachment_id.get(), "same"),
            other => panic!("complete 后应命中缓存: {other:?}"),
        }
    }

    #[test]
    fn concurrent_mixed_operations_do_not_panic_or_cross_attach() {
        // AC-004-09：多线程混跑 acquire/get/complete，无 panic、无串图
        // （attachment_id → 字节数一一对应）。
        let cache = Arc::new(ImageCache::new(100_000));
        let dir = cache.temp_dir();
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let cache = cache.clone();
            let dir = dir.clone();
            handles.push(thread::spawn(move || {
                for i in 0..30u64 {
                    let id = AttachmentId::new(format!("img-{}", (t * 100 + i) % 5));
                    match cache.acquire(&id) {
                        Acquire::Started => {
                            let e = entry(&format!("img-{}", (t * 100 + i) % 5), 64, &dir);
                            cache.complete(&id, e);
                        }
                        Acquire::InFlight | Acquire::Cached(_) => {}
                    }
                    if let Some(e) = cache.get(&id) {
                        // 串图检测：缓存字节数必须与文件实际内容一致。
                        let meta = std::fs::metadata(&e.temp_file).unwrap();
                        assert_eq!(meta.len(), e.bytes);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(cache.used() <= 100_000);
    }

    #[test]
    fn temp_file_extension_follows_media_type_and_drop_cleans_dir() {
        let dir;
        {
            let cache = ImageCache::new(10_000);
            dir = cache.temp_dir();
            let path = cache
                .write_temp_file(&MediaType::new("image/gif".into()), b"GIF89a".to_vec())
                .unwrap();
            assert!(path.extension().unwrap() == "gif", "path={path:?}");
            assert!(path.exists());
        }
        assert!(!dir.exists(), "Drop 必须清理临时目录（进程退出清理）");
    }
}
