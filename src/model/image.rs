//! REQ-004 V0.2 图片模型：AttachmentRef（wire→内部）、ImageCacheEntry、
//! ImageViewState、mediaType 白名单与 `Block::Image` 消费 helper。
//!
//! 字段表口径（REQ-004 §5，D-15）：wire 命名以官方 Remote API 实读
//! v0.1.2-alpha.5 为准，内部一律 snake_case；`attachmentId` 仅作
//! `session/attachment` wire 参数名，内部模型/缓存键统一 `attachment_id`
//! （FR-004-01 事实修正）。纯同步、无 reqwest/ratatui 依赖（02 §2 分层）。

use std::path::PathBuf;

use crate::api::attachment::AttachmentData;
use crate::api::types::{AttachmentId, MediaType, SessionSeq};
use crate::model::Block;

/// mediaType 白名单（REQ-004 §5/§7：白名单外不解码渲染，进错误占位）。
pub const SUPPORTED_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// 白名单判定：仅 png/jpeg/webp/gif 可解码渲染（§7 安全边界）。
pub fn is_supported_image(media_type: &str) -> bool {
    SUPPORTED_IMAGE_TYPES.contains(&media_type)
}

/// `Block::Image` 的 identity 快照（V0.1 已交付，REQ-001 §4；本 REQ 消费）。
/// 打开 ImageView 时作为防串图锚点（来源 `Block.seq`，D-14/§5）。
#[derive(Debug, Clone, PartialEq)]
pub struct ImageBlockRef {
    pub seq: SessionSeq,
    pub attachment_id: Option<AttachmentId>,
    pub name: Option<String>,
    pub dims: Option<String>,
}

/// 从 `Block::Image` 提取 identity；非图片块返回 None（逐块独立占位，
/// AC-004-01）。
pub fn image_block_of(block: &Block) -> Option<ImageBlockRef> {
    match block {
        Block::Image {
            seq,
            attachment_id,
            name,
            dims,
        } => Some(ImageBlockRef {
            seq: *seq,
            attachment_id: attachment_id
                .as_deref()
                .map(|id| AttachmentId(id.to_string())),
            name: name.clone(),
            dims: dims.clone(),
        }),
        _ => None,
    }
}

/// `session/attachment` 响应 → 内部模型（wire camelCase → snake_case，
/// REQ-004 §5 字段表）。
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentRef {
    pub attachment_id: AttachmentId,
    pub media_type: MediaType,
    /// 编码字节数（缓存预算记账口径）。
    pub bytes: u64,
    /// 编码固有尺寸。
    pub width: u64,
    pub height: u64,
    /// 显示名（官方已剥离路径信息）。
    pub name: Option<String>,
    /// 规范化缩放前的输入尺寸（可空）。
    pub original_dimensions: Option<OriginalDimensions>,
}

/// `originalDimensions`（wire）→ 内部 snake_case。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginalDimensions {
    pub width: u64,
    pub height: u64,
}

impl From<&AttachmentData> for AttachmentRef {
    fn from(data: &AttachmentData) -> Self {
        Self {
            attachment_id: data.attachment_id.clone(),
            media_type: data.media_type.clone(),
            bytes: data.bytes,
            width: data.width,
            height: data.height,
            name: data.name.clone(),
            original_dimensions: data
                .original_dimensions
                .as_ref()
                .map(|d| OriginalDimensions {
                    width: d.width,
                    height: d.height,
                }),
        }
    }
}

/// LRU 缓存条目（REQ-004 §5：`attachment_id → 解码后尺寸 + 临时文件`；
/// 预算总账 = Σ(`bytes` + `temp_file` 占用) ≤ `cache_bytes`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCacheEntry {
    pub attachment_id: AttachmentId,
    pub media_type: MediaType,
    /// 编码字节数（预算记账）。
    pub bytes: u64,
    /// 解码后尺寸（渲染/降采样用）。
    pub width: u64,
    pub height: u64,
    /// 系统临时目录文件（随机名），进程退出清理（06 §9 不落盘会话内容）。
    pub temp_file: PathBuf,
    /// 单调时间戳（LRU 序——超预算时驱逐最旧）。
    pub last_used: u64,
}

/// ImageView 渲染阶段（REQ-004 §5 状态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageViewPhase {
    /// 未打开（含关闭后）。
    #[default]
    Closed,
    /// 拉取/解码在途。
    Loading,
    /// Kitty 帧已就绪。
    Rendered,
    /// 拉取/解码失败（错误占位）。
    Failed,
}

/// Failed 时的错误（Remote `error.code` 或解码错误，§5/§6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageViewError {
    pub code: String,
    pub message: String,
}

/// REQ-007 AC-007-06/30：同消息多图 pager（additive，仅 Kitty ImageView）。
#[derive(Debug, Clone, PartialEq)]
pub struct ImagePager {
    /// 同一消息内连续图片块总数。
    pub total: usize,
    /// 当前图在组内下标（0-based）。
    pub index: usize,
}

/// ImageView 状态（REQ-004 §5 字段表；仅 Kitty 渲染态出现）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageViewState {
    pub open: bool,
    /// 来源 `Block::Image.seq`（防串图锚点）。
    pub block_seq: Option<SessionSeq>,
    pub attachment_id: Option<AttachmentId>,
    pub name: Option<String>,
    pub dims: Option<String>,
    pub phase: ImageViewPhase,
    pub error: Option<ImageViewError>,
    /// REQ-007：同消息多图 pager（Some = 组内可 [ ] 切换；None = 单图）。
    pub pager: Option<ImagePager>,
}

/// 计算图片组（同一消息内连续 Image 块的 run）：返回 (run_start, total,
/// current_index)。
pub fn image_run_of(blocks: &[Block], seq: SessionSeq) -> Option<(usize, usize, usize)> {
    let is_img = |b: &Block| matches!(b, Block::Image { .. });
    let pos = blocks.iter().position(|b| b.seq() == seq && is_img(b))?;
    let mut start = pos;
    while start > 0 && is_img(&blocks[start - 1]) {
        start -= 1;
    }
    let mut end = start;
    while end < blocks.len() && is_img(&blocks[end]) {
        end += 1;
    }
    Some((start, end - start, pos - start))
}

impl ImageViewState {
    /// 打开视图：记录来源块锚点与目标附件，进入 Loading（幂等由 app 层
    /// 在途/已开判定兜底，AC-004-08）。
    pub fn open_view(
        &mut self,
        block_seq: SessionSeq,
        attachment_id: AttachmentId,
        name: Option<String>,
        dims: Option<String>,
    ) {
        self.open = true;
        self.block_seq = Some(block_seq);
        self.attachment_id = Some(attachment_id);
        self.name = name;
        self.dims = dims;
        self.phase = ImageViewPhase::Loading;
        self.error = None;
    }

    pub fn mark_rendered(&mut self) {
        self.phase = ImageViewPhase::Rendered;
        self.error = None;
    }

    pub fn mark_failed(&mut self, code: String, message: String) {
        self.phase = ImageViewPhase::Failed;
        self.error = Some(ImageViewError { code, message });
    }

    /// 关闭回 transcript（NORMAL，AC-004-05）。
    pub fn close(&mut self) {
        self.open = false;
        self.block_seq = None;
        self.attachment_id = None;
        self.name = None;
        self.dims = None;
        self.phase = ImageViewPhase::Closed;
        self.error = None;
        self.pager = None;
    }

    /// 打开时记录同消息组 pager（多图切换 `[`/`]`）。
    pub fn set_pager(&mut self, total: usize, index: usize) {
        self.pager = Some(ImagePager {
            total: total.max(1),
            index: index.min(total.saturating_sub(1)),
        });
    }

    /// pager 步进（delta ±1；返回是否可再走）。
    pub fn move_pager(&mut self, delta: i8) -> bool {
        let Some(p) = self.pager.as_mut() else {
            return false;
        };
        let next = p.index as isize + delta as isize;
        if next < 0 || next as usize >= p.total {
            return false;
        }
        p.index = next as usize;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SessionSeq;

    #[test]
    fn supported_whitelist_and_rejects() {
        for ok in SUPPORTED_IMAGE_TYPES {
            assert!(is_supported_image(ok));
        }
        assert!(!is_supported_image("image/svg+xml"));
        assert!(!is_supported_image(""));
    }

    #[test]
    fn image_run_of_groups_contiguous_images_ac007_06() {
        use crate::api::types::{ChunkData, ChunkRow, SessionSeq as Seq};
        use crate::model::PackedChunks;
        let img = |seq: u64| Block::Image {
            seq: Seq(seq),
            attachment_id: Some(format!("a{seq}")),
            name: None,
            dims: None,
        };
        let blocks = vec![
            img(1),
            img(2),
            Block::UserMessage {
                seq: Seq(3),
                content: "之间".into(),
                time: None,
            },
            img(4),
            img(5),
            img(6),
        ];
        // seq=1 组: (start0,total2,idx0)
        assert_eq!(image_run_of(&blocks, Seq(1)), Some((0, 2, 0)));
        // seq=2 → (0,2,1)
        assert_eq!(image_run_of(&blocks, Seq(2)), Some((0, 2, 1)));
        // seq=5 → 组在 idx3..6 (start3,total3,idx1)
        assert_eq!(image_run_of(&blocks, Seq(5)), Some((3, 3, 1)));
        // 非图片 seq 不命中
        assert_eq!(image_run_of(&blocks, Seq(3)), None);
        // 越界/缺失 seq → None
        assert_eq!(image_run_of(&blocks, Seq(99)), None);
        let _ = PackedChunks::default();
        let _ = ChunkData::default();
        let _ = ChunkRow::Unknown {
            event_type: String::new(),
            raw: serde_json::Value::Null,
        };
    }

    #[test]
    fn image_view_pager_move_and_close_clears_ac007_06() {
        let mut v = ImageViewState {
            open: true,
            ..Default::default()
        };
        v.set_pager(3, 1);
        assert_eq!(v.pager.as_ref().map(|p| (p.total, p.index)), Some((3, 1)));
        assert!(v.move_pager(1));
        assert_eq!(v.pager.as_ref().map(|p| p.index), Some(2));
        assert!(!v.move_pager(1), "到组尾不能再走");
        assert!(v.move_pager(-1));
        assert!(v.move_pager(-1));
        assert_eq!(v.pager.as_ref().map(|p| p.index), Some(0));
        v.close();
        assert!(v.pager.is_none(), "close 清 pager");
    }

    #[test]
    fn image_block_of_none_for_non_image_blocks() {
        let block = Block::Unknown {
            seq: SessionSeq(1),
            event_type: "weird/thing".into(),
            raw: serde_json::Value::Null,
        };
        assert!(image_block_of(&block).is_none());
    }
}
