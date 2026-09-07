//! Agent Town 动画画布渲染器（REQ-009 §5 `CanvasState`，D-28 自包含新增 ui
//! 模块，不扩展 REQ-004 已交付管线）。
//!
//! - RGBA 双缓冲 960×540 → 静态背景 `a=T,f=32` 一次传输 → 后续帧
//!   kitty `a=f` 部分矩形增量（首帧 `c=1` 从根帧合成第 2 帧，之后 `r=2`
//!   就地编辑第 2 帧，`X=1` 替换，`C=1` 光标右移）+ `a=a,c=2` 显示当前帧；
//! - 静态场景（无脏区）不发帧（`paint → Static`，AC-009-03 停帧口径）；
//! - `Widget for &mut TownCanvas` 把 kitty 字节写 backend queue：cell 符号
//!   变化时 ratatui 才重发，静态时符号不变即零输出；
//! - 非 kitty 终端不实例化（上层走文本 roster 降级，D-30）。
//!
//! kitty 协议依据：本机 `/usr/share/doc/kitty/html/graphics-protocol.html`
//! Animation 节（a=f/c/r/X/C/z 键语义）。⚠️ 真机显示验证受会话环境限制
//! （headless 沙箱无 GL 上下文），字节级编码与帧率由原型实测（见 TASK
//! `## 实现记录`）。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ratatui::layout::Rect;

use crate::model::agent_town::{TownScene, TOWN_H, TOWN_W};

/// 压缩阈值：payload 超过该尺寸才启用 `o=z`（小脏矩形压缩收益 < deflate
/// 开销，且 tiny 帧零压缩保持简单）。
const COMPRESS_THRESHOLD: usize = 64 * 1024;

/// kitty 分块传输（4000 字节/块，`m=1` 续块、`m=0` 末块，vendored
/// ratatui-image kitty.rs:153-175 同口径）。`compressed=true` 时首块声明
/// `o=z`（kitty 端自动解压；graphics-protocol.html）。
/// 输出缓冲复用（避免每帧大分配 churn 被 glibc arena 滞留，RSS 口径）。
fn chunked_into(out: &mut String, control: &str, data: &[u8], compressed: bool) {
    out.clear();
    out.reserve(data.len() + data.len() / 3 + 64);
    let chunks: Vec<&[u8]> = data.chunks(4000).collect();
    let n = chunks.len();
    for (i, ch) in chunks.iter().enumerate() {
        let more = if i + 1 < n { 1 } else { 0 };
        out.push_str("\x1b_G");
        if i == 0 {
            out.push_str(control);
            if compressed {
                out.push_str(",o=z");
            }
            out.push_str(",m=");
        } else {
            out.push_str("m=");
        }
        out.push_str(if more == 1 { "1" } else { "0" });
        out.push(';');
        STANDARD.encode_string(ch, out);
        out.push_str("\x1b\\");
    }
}

/// 行缓冲 RGBA 提取（复用 rowbuf，避免整幅 payload 暂存 → RSS 口径）。
fn rect_rgba_row(rowbuf: &mut [u8], buf: &[u32], row: usize, x: u32, w: u32) {
    let mut i = 0usize;
    for col in x..x + w {
        let v = buf[row * TOWN_W + col as usize].to_le_bytes();
        rowbuf[i..i + 4].copy_from_slice(&v);
        i += 4;
    }
}

/// 流式 deflate（fast 级别，miniz_oxide 纯 Rust 后端）：按行把
/// buf 的矩形区域喂给编码器，输出进复用缓冲 zbuf。
fn deflate_rect(
    zbuf: &mut Vec<u8>,
    rowbuf: &mut [u8],
    buf: &[u32],
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) {
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;
    zbuf.clear(); // DeflateEncoder 从缓冲当前长度续写，必须先清空
    let owned = std::mem::take(zbuf);
    let mut enc = DeflateEncoder::new(owned, Compression::fast());
    for row in y..y + h {
        rect_rgba_row(rowbuf, buf, row as usize, x, w);
        let _ = enc.write_all(&rowbuf[..(w * 4) as usize]);
    }
    *zbuf = enc.finish().unwrap_or_default();
}

/// 全量首帧（静态背景一次传输 + 显示）：流式 deflate（o=z）。
fn encode_full_into(out: &mut String, zbuf: &mut Vec<u8>, rowbuf: &mut [u8], buf: &[u32], id: u32) {
    deflate_rect(zbuf, rowbuf, buf, 0, 0, TOWN_W as u32, TOWN_H as u32);
    let ctrl = format!("q=2,i={id},a=T,f=32,s={TOWN_W},v={TOWN_H}");
    chunked_into(out, &ctrl, zbuf, true);
}

/// 部分矩形增量帧：`a=f` + `c=1`（首帧从根帧合成第 2 帧）/ `r=2`（就地
/// 编辑第 2 帧）+ `X=1` 简单替换 + `C=1` 光标右移；随后 `a=a,c=2` 把第 2
/// 帧设为当前帧（client-driven 动画，graphics-protocol.html Animation 节）。
/// 大矩形流式 deflate（o=z）；小矩形（<64KB）零压缩直接分块。
#[allow(clippy::too_many_arguments)] // 编码上下文单一调用点，main.rs 同款豁免
fn encode_delta_into(
    out: &mut String,
    zbuf: &mut Vec<u8>,
    rowbuf: &mut [u8],
    small: &mut Vec<u8>,
    buf: &[u32],
    id: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    first: bool,
) {
    let src = if first { "c=1" } else { "r=2" };
    let ctrl = format!("q=2,i={id},a=f,{src},x={x},y={y},s={w},v={h},X=1,C=1");
    let len = (w * h * 4) as usize;
    if len > COMPRESS_THRESHOLD {
        deflate_rect(zbuf, rowbuf, buf, x, y, w, h);
        chunked_into(out, &ctrl, zbuf, true);
    } else {
        small.clear();
        small.reserve(len);
        for row in y..y + h {
            rect_rgba_row(rowbuf, buf, row as usize, x, w);
            small.extend_from_slice(&rowbuf[..(w * 4) as usize]);
        }
        chunked_into(out, &ctrl, small, false);
    }
    out.push_str(&format!("\x1b_Gq=2,i={id},a=a,c=2\x1b\\"));
}

/// `paint()` 结果：Full（首帧/强制重绘）、Delta（脏矩形增量）、Static（停帧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintOutcome {
    Full,
    Delta { x: u32, y: u32, w: u32, h: u32 },
    Static,
}

/// 动画画布（CanvasState：双缓冲 + 脏矩形 + 静态停帧）。
pub struct TownCanvas {
    /// 当前已发送帧内容。
    front: Vec<u32>,
    /// 新帧渲染目标。
    back: Vec<u32>,
    static_sent: bool,
    /// 第 2 帧（增量目标）是否已创建（首增量用 `c=1`，其后 `r=2`）。
    frame_created: bool,
    image_id: u32,
    frame_seq: u64,
    /// 待发送帧字节（take-once：由事件循环取走直接写 backend，随后
    /// 归还复用——不驻留 ratatui Buffer，控制 RSS，AC-009-09）。
    emit: Option<String>,
    /// deflate 输出缓冲（复用）。
    zbuf: Vec<u8>,
    /// 行 RGBA 提取缓冲（960 像素 × 4B，复用）。
    rowbuf: [u8; TOWN_W * 4],
    /// 小脏矩形（<64KB 零压缩路径）payload 缓冲（复用）。
    small: Vec<u8>,
    /// 编码失败降级标志（渲染失败不崩溃，AC-009-08 精神）。
    pub degraded: bool,
}

impl TownCanvas {
    pub fn new(image_id: u32) -> Self {
        Self {
            front: vec![0u32; TOWN_W * TOWN_H],
            back: vec![0u32; TOWN_W * TOWN_H],
            static_sent: false,
            frame_created: false,
            image_id,
            frame_seq: 0,
            emit: None,
            zbuf: Vec::new(),
            rowbuf: [0u8; TOWN_W * 4],
            small: Vec::new(),
            degraded: false,
        }
    }

    /// 光栅化场景 → 双缓冲 diff → 脏矩形 → 编码字节。
    /// `Static` = 无变化（上层跳过发送，零输出）。
    pub fn paint(&mut self, scene: &TownScene) -> PaintOutcome {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        scene.render(&mut self.back);
        let mut dirty = dirty_bounds(&self.front, &self.back);
        if !self.static_sent {
            dirty = Some((0, 0, TOWN_W as u32, TOWN_H as u32));
        }
        let outcome = match dirty {
            None => PaintOutcome::Static,
            Some((x, y, w, h)) => {
                // 复用上一帧的 String 缓冲（write 完成后由循环归还）。
                let mut out = self.emit.take().unwrap_or_default();
                let arm = if !self.static_sent {
                    encode_full_into(
                        &mut out,
                        &mut self.zbuf,
                        &mut self.rowbuf,
                        &self.back,
                        self.image_id,
                    );
                    self.static_sent = true;
                    PaintOutcome::Full
                } else {
                    let first = !self.frame_created;
                    self.frame_created = true;
                    encode_delta_into(
                        &mut out,
                        &mut self.zbuf,
                        &mut self.rowbuf,
                        &mut self.small,
                        &self.back,
                        self.image_id,
                        x,
                        y,
                        w,
                        h,
                        first,
                    );
                    PaintOutcome::Delta { x, y, w, h }
                };
                self.emit = Some(out);
                arm
            }
        };
        if !matches!(outcome, PaintOutcome::Static) {
            // front ← 已发送内容（交换避免整块拷贝；back 变旧 front 下次覆盖）。
            std::mem::swap(&mut self.front, &mut self.back);
        }
        outcome
    }

    /// resize/重连/恢复：强制下次 paint 重发全量首帧。
    pub fn force_full_redraw(&mut self) {
        self.static_sent = false;
        self.frame_created = false;
        self.emit = None;
    }

    /// 取走待发送帧字节（take-once；无新帧返回 None）。
    pub fn take_emit(&mut self) -> Option<String> {
        self.emit.take()
    }

    /// 归还帧字节缓冲（write 完成后调用；缓冲容量复用，避免重复大分配）。
    pub fn put_back_emit(&mut self, mut bytes: String) {
        bytes.clear();
        if self.emit.is_none() {
            self.emit = Some(bytes);
        }
    }

    /// 帧序号（诊断/测试用）。
    pub fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    /// 显示区域尺寸（cells）：960×540 原生像素按终端 cell 像素折算。
    /// base 帧不指定 `c`/`r` → 原生像素显示；映射口径与
    /// `cell_to_logical` 共用（字体 cell 尺寸决定每 cell 覆盖的像素）。
    pub fn display_cells(font: (u16, u16)) -> (u16, u16) {
        let cw = font.0.max(1) as u32;
        let ch = font.1.max(1) as u32;
        let cols = (TOWN_W as u32).div_ceil(cw);
        let rows = (TOWN_H as u32).div_ceil(ch);
        (cols as u16, rows as u16)
    }

    /// 显示 cell → 逻辑像素（最近邻映射，供鼠标命中判定，AC-009-04）。
    /// 画面锚定于 area 左上角；超出画面覆盖范围返回 None。
    pub fn cell_to_logical(area: Rect, font: (u16, u16), col: u16, row: u16) -> Option<(u16, u16)> {
        if col < area.left() || row < area.top() {
            return None;
        }
        let cw = font.0.max(1) as u32;
        let ch = font.1.max(1) as u32;
        let cols = (TOWN_W as u32).div_ceil(cw).max(1);
        let rows = (TOWN_H as u32).div_ceil(ch).max(1);
        let dc = (col - area.left()) as u32;
        let dr = (row - area.top()) as u32;
        if dc >= cols || dr >= rows {
            return None;
        }
        Some((
            (dc * TOWN_W as u32 / cols) as u16,
            (dr * TOWN_H as u32 / rows) as u16,
        ))
    }
}

/// 前后帧 diff → 覆盖全部差异像素的最小包围矩形。
fn dirty_bounds(front: &[u32], back: &[u32]) -> Option<(u32, u32, u32, u32)> {
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    for idx in 0..front.len().min(back.len()) {
        if front[idx] == back[idx] {
            continue;
        }
        let x = (idx % TOWN_W) as u32;
        let y = (idx / TOWN_W) as u32;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    if min_x == u32::MAX {
        None
    } else {
        Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::agent_roster::AgentRosterEntry;
    use crate::model::agent_town::TownScene;

    #[test]
    fn full_frame_contains_transmit_and_rgba_format() {
        let mut c = TownCanvas::new(7);
        let scene = TownScene::new(1, 0.0);
        let out = c.paint(&scene);
        assert_eq!(out, PaintOutcome::Full);
        let bytes = c.take_emit().expect("首帧应有字节");
        assert!(bytes.contains("a=T"), "首帧须含 a=T: {:?}", &bytes[..40]);
        assert!(bytes.contains("f=32"), "首帧须含 f=32");
        assert!(bytes.contains("s=960"), "首帧须含 s=960");
        assert!(bytes.contains("v=540"), "首帧须含 v=540");
        assert!(bytes.contains("i=7"), "首帧须含 image id");
        assert!(!bytes.contains("a=f"), "首帧不是增量帧");
        assert!(c.take_emit().is_none(), "take-once 语义");
    }

    #[test]
    fn full_frame_is_deflate_compressed_with_o_z() {
        let mut c = TownCanvas::new(7);
        let scene = TownScene::new(1, 0.0);
        c.paint(&scene);
        let bytes = c.take_emit().unwrap();
        assert!(bytes.contains("o=z"), "大帧须启用 kitty o=z 压缩");
        assert!(
            bytes.len() < 2_700_000,
            "压缩后全量帧应显著小于未压缩 2.7MB: {}",
            bytes.len()
        );
        // 解压回读：数据可逆（flate2 解码）。
        use std::io::Read;
        let mut decoded = Vec::new();
        for chunk in bytes.split("\x1b_G").skip(1) {
            let Some(ctrl_end) = chunk.find(';') else {
                continue;
            };
            let Some(payload_end) = chunk.find("\x1b\\") else {
                continue;
            };
            let ctrl = &chunk[..ctrl_end];
            if ctrl.starts_with("q=2,i=7,a=a") {
                continue;
            }
            decoded.extend(STANDARD.decode(&chunk[ctrl_end + 1..payload_end]).unwrap());
        }
        let mut inflater = flate2::read::DeflateDecoder::new(decoded.as_slice());
        let mut raw = Vec::new();
        inflater.read_to_end(&mut raw).unwrap();
        assert_eq!(raw.len(), TOWN_W * TOWN_H * 4, "解压后为全量 RGBA");
    }

    #[test]
    fn delta_frames_use_c1_then_r2_and_display_control() {
        let mut c = TownCanvas::new(3);
        let mut scene = TownScene::new(2, 0.0);
        assert_eq!(c.paint(&scene), PaintOutcome::Full);
        let _ = c.take_emit();
        // 推进时间产生动画差异（水面/喷泉动态）。
        scene.advance(200.0);
        let out = c.paint(&scene);
        let (x, y, w, h) = match out {
            PaintOutcome::Delta { x, y, w, h } => (x, y, w, h),
            other => panic!("应有增量帧，实际 {other:?}"),
        };
        let bytes = c.take_emit().expect("增量帧应有字节");
        assert!(bytes.contains("a=f"), "增量帧须含 a=f");
        assert!(bytes.contains("C=1"), "增量帧须含 C=1（计划字节断言）");
        assert!(
            bytes.contains("c=1"),
            "首个增量用 c=1 合成: {:?}",
            &bytes[..60]
        );
        assert!(bytes.contains(&format!("x={x},y={y},s={w},v={h}")));
        assert!(bytes.contains("a=a,c=2"), "须含显示控制 a=a,c=2");

        // 第二个增量：r=2 就地编辑。
        scene.advance(300.0);
        let out2 = c.paint(&scene);
        assert!(matches!(out2, PaintOutcome::Delta { .. }));
        let bytes2 = c.take_emit().expect("第二个增量应有字节");
        assert!(
            bytes2.contains("r=2"),
            "后续增量用 r=2 编辑: {:?}",
            &bytes2[..60]
        );
        assert!(!bytes2.contains("c=1"));
    }

    #[test]
    fn delta_payload_decodes_to_dirty_rect_rgba() {
        let mut c = TownCanvas::new(5);
        let mut scene = TownScene::new(4, 0.0);
        c.paint(&scene);
        let _ = c.take_emit();
        scene.advance(150.0);
        let (x, y, w, h) = match c.paint(&scene) {
            PaintOutcome::Delta { x, y, w, h } => (x, y, w, h),
            other => panic!("期望 Delta，实际 {other:?}"),
        };
        // 提取 base64 payload（按 chunk 重组：每块 \x1b_G..;<b64>\x1b\，最后为 a=a）。
        let bytes = c.take_emit().unwrap();
        let mut decoded = Vec::new();
        let mut compressed = false;
        for chunk in bytes.split("\x1b_G").skip(1) {
            let Some(ctrl_end) = chunk.find(';') else {
                continue;
            };
            let Some(payload_end) = chunk.find("\x1b\\") else {
                continue;
            };
            let ctrl = &chunk[..ctrl_end];
            if ctrl.starts_with("q=2,i=5,a=a") {
                continue; // 显示控制无 payload
            }
            if ctrl.contains("o=z") {
                compressed = true;
            }
            decoded.extend(STANDARD.decode(&chunk[ctrl_end + 1..payload_end]).unwrap());
        }
        if compressed {
            use std::io::Read;
            let mut inflater = flate2::read::DeflateDecoder::new(decoded.as_slice());
            let mut raw = Vec::new();
            inflater.read_to_end(&mut raw).unwrap();
            decoded = raw;
        }
        assert_eq!(decoded.len(), (w * h * 4) as usize, "payload 为脏矩形 RGBA");
        // 与已发送帧（paint 后 front = 最新帧）对应区域逐字节一致。
        let front = &c.front;
        for row in y..y + h {
            for col in x..x + w {
                let off = ((row - y) * w + (col - x)) as usize * 4;
                let expect = front[row as usize * TOWN_W + col as usize].to_le_bytes();
                assert_eq!(&decoded[off..off + 4], &expect, "像素 ({col},{row}) 不一致");
            }
        }
    }

    #[test]
    fn small_delta_stays_uncompressed_and_large_is_compressed() {
        let mut c = TownCanvas::new(17);
        let mut scene = TownScene::new(16, 0.0);
        let e = AgentRosterEntry::from_wire(&crate::api::monitor::WireAgent {
            session_id: "session-s".into(),
            phase: "".into(),
            task: "t".into(),
            project: "p".into(),
            task_id: "".into(),
            status: "idle".into(),
            task_status: "".into(),
            elapsed: 1,
            last_event_at: 0,
            seq: 1,
            label: "".into(),
            kind: "session".into(),
            parent_session_id: "".into(),
            delegation_depth: 0,
            provider: "p".into(),
            model: "m".into(),
        });
        scene.sync_agents(&[e]);
        c.paint(&scene);
        let _ = c.take_emit();
        // 单 NPC 小位移 → 小脏矩形 → 不压缩（阈值 64KB 之下）。
        if let Some(n) = scene.npc_mut(&crate::api::types::SessionId("session-s".into())) {
            n.cx += 8.0;
            n.cy -= 4.0;
        }
        let (x, y, w, h) = match c.paint(&scene) {
            PaintOutcome::Delta { x, y, w, h } => (x, y, w, h),
            other => panic!("期望 Delta，实际 {other:?}"),
        };
        assert!(
            (w as usize) * (h as usize) * 4 < COMPRESS_THRESHOLD,
            "小脏矩形"
        );
        let bytes = c.take_emit().unwrap();
        assert!(!bytes.contains("o=z"), "小帧不压缩");
        let _ = (x, y);
    }

    #[test]
    fn static_scene_stops_emitting_frames() {
        let mut c = TownCanvas::new(9);
        let scene = TownScene::new(6, 0.0);
        assert_eq!(c.paint(&scene), PaintOutcome::Full);
        let _ = c.take_emit();
        // 同时间戳重画：内容无变化 → Static（停帧，AC-009-03）。
        assert_eq!(c.paint(&scene), PaintOutcome::Static);
        assert!(c.take_emit().is_none(), "静态时零输出");
    }

    #[test]
    fn force_full_redraw_resends_base_frame() {
        let mut c = TownCanvas::new(11);
        let mut scene = TownScene::new(8, 0.0);
        c.paint(&scene);
        let _ = c.take_emit();
        scene.advance(200.0);
        assert!(matches!(c.paint(&scene), PaintOutcome::Delta { .. }));
        let _ = c.take_emit();
        c.force_full_redraw();
        assert_eq!(c.paint(&scene), PaintOutcome::Full);
        assert!(c.take_emit().unwrap().contains("a=T"));
    }

    #[test]
    fn cell_to_logical_maps_display_cells_to_pixels() {
        let area = Rect::new(2, 1, 120, 34);
        // 8×16 字体：120×34 cells 覆盖 960×540（每 cell 8×16 逻辑像素）。
        assert_eq!(
            TownCanvas::cell_to_logical(area, (8, 16), 2, 1),
            Some((0, 0))
        );
        assert_eq!(
            TownCanvas::cell_to_logical(area, (8, 16), 62, 18),
            Some((480, 270))
        );
        assert_eq!(
            TownCanvas::cell_to_logical(area, (8, 16), 121, 34),
            Some((952, 524))
        );
        // 越界：画面覆盖范围外 → None（命中判定裁剪）。
        assert_eq!(TownCanvas::cell_to_logical(area, (8, 16), 122, 1), None);
        assert_eq!(TownCanvas::cell_to_logical(area, (8, 16), 1, 1), None);
        assert_eq!(TownCanvas::cell_to_logical(area, (8, 16), 10, 35), None);
        // 大字体（16px cell）：60×34 cells → 16 逻辑像素/cell。
        assert_eq!(
            TownCanvas::cell_to_logical(area, (16, 16), 2 + 30, 1 + 17),
            Some((480, 270))
        );
        assert_eq!(TownCanvas::display_cells((8, 16)), (120, 34));
        assert_eq!(TownCanvas::display_cells((16, 16)), (60, 34));
    }

    #[test]
    fn render_with_roster_agent_in_scene_produces_delta_on_move() {
        // 端到端：agent 入镇 → 首帧 → 时间推进 → 增量（NPC 移动）。
        let mut c = TownCanvas::new(15);
        let mut scene = TownScene::new(12, 0.0);
        let e = AgentRosterEntry::from_wire(&crate::api::monitor::WireAgent {
            session_id: "session-m".into(),
            phase: "".into(),
            task: "t".into(),
            project: "p".into(),
            task_id: "".into(),
            status: "working".into(),
            task_status: "implementing".into(),
            elapsed: 1,
            last_event_at: 0,
            seq: 1,
            label: "".into(),
            kind: "task".into(),
            parent_session_id: "".into(),
            delegation_depth: 0,
            provider: "p".into(),
            model: "m".into(),
        });
        scene.sync_agents(&[e]);
        assert_eq!(c.paint(&scene), PaintOutcome::Full);
        scene.advance(1000.0);
        let out = c.paint(&scene);
        // 人物移动或水面/喷泉动画 → 增量；静态判定仅当完全无变化。
        assert!(
            matches!(out, PaintOutcome::Delta { .. }),
            "时间推进后应有增量: {out:?}"
        );
    }
}
