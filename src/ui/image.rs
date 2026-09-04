//! 图片管线（REQ-004 §4；Notes/06 §6）：字节 → 解码 RGBA（gif 仅首帧）→
//! 降采样到视口 → ratatui-image(kitty) 编码 → Kitty graphics protocol 帧。
//! 能力检测 $TERM/$KITTY_WINDOW_ID（06 §6）。
//!
//! - 解码/降采样/编码为纯同步函数，调用方（main.rs）置于 `spawn_blocking`
//!   执行，不阻塞 TUI 帧循环（06 §5/§7）；
//! - gif 仅首帧：image 0.24 GifDecoder 默认只解第一帧（原型已验证，不卡顿）；
//! - 错误可读 code/message（REQ-004 §6：格式/损坏不自动重试，进 tracing）。

use image::RgbaImage;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::protocol::{ImageSource, Protocol};
use ratatui_image::Resize;

use crate::api::types::MediaType;
use crate::model::is_supported_image;

/// 解码/编码错误（code 供分类：decode/unsupported、decode/corrupt、
/// encode/failed；REQ-004 §6 错误模型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDecodeError {
    pub code: String,
    pub message: String,
}

/// 解码结果：RGBA 像素 + 固有尺寸。
#[derive(Debug)]
pub struct DecodedImage {
    pub rgba: RgbaImage,
    pub width: u32,
    pub height: u32,
}

/// 字节 → RGBA 解码。mediaType 白名单外/字节损坏 → 可读错误
/// （不自动重试，AC-004-06/10）。
pub fn decode_image(
    bytes: &[u8],
    media_type: &MediaType,
) -> Result<DecodedImage, ImageDecodeError> {
    if !is_supported_image(&media_type.0) {
        return Err(ImageDecodeError {
            code: "decode/unsupported".into(),
            message: format!("不支持的媒体类型 {}", media_type.0),
        });
    }
    let img = image::load_from_memory(bytes).map_err(|e| ImageDecodeError {
        code: "decode/corrupt".into(),
        message: format!("图片解码失败: {e}"),
    })?;
    let rgba = img.to_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        rgba,
    })
}

/// Kitty 能力检测（Notes/06 §6）：`$TERM` 含 `kitty` 或 `$KITTY_WINDOW_ID`
/// 存在。启动/打开会话时检测一次。
pub fn kitty_supported() -> bool {
    let term = std::env::var("TERM").ok();
    let window_id = std::env::var("KITTY_WINDOW_ID").ok();
    kitty_supported_with(term.as_deref(), window_id.as_deref())
}

/// 纯函数版本（测试 seam）。
pub fn kitty_supported_with(term: Option<&str>, kitty_window_id: Option<&str>) -> bool {
    term.is_some_and(|t| t.to_ascii_lowercase().contains("kitty")) || kitty_window_id.is_some()
}

/// 终端 cell 像素大小（ioctl 探测，失败回退默认 (8,16)）。
pub fn terminal_font_size() -> (u16, u16) {
    Picker::from_termios()
        .map(|p| p.font_size)
        .unwrap_or((8, 16))
}

/// 大图降采样到视口 + kitty 编码（ratatui-image 0.10 kitty 后端；
/// `Resize::Fit` 保持宽高比，原型已验证）。`id` 为 kitty image id（每帧唯一，
/// 由调用方分配）。
pub fn kitty_frame(
    image: image::DynamicImage,
    font_size: (u16, u16),
    area: Rect,
    id: u8,
) -> Result<Box<dyn Protocol>, ImageDecodeError> {
    let source = ImageSource::new(image, font_size);
    Kitty::from_source(&source, Resize::Fit(None), None, area, id)
        .map(|k| Box::new(k) as Box<dyn Protocol>)
        .map_err(|e| ImageDecodeError {
            code: "encode/failed".into(),
            message: format!("kitty 帧编码失败: {e}"),
        })
}

// ---------- 降级链（ADR-005：Kitty → 占位框 + 路径提示） ----------
//
// V0.2 消息流恒为占位（D-13）：非 Kitty 消息流用文本占位 + 路径提示，
// 打开走系统查看器；占位框用于 ImageView 的错误占位（AC-004-03/06）。

/// 占位框 + 路径提示（降级链最末级；亦用于错误占位，AC-004-03/06）。
/// 内容为可读提示行，绝不 panic。
pub fn render_placeholder_box(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    lines: &[String],
) {
    let text: Vec<Line<'static>> = lines
        .iter()
        .map(|l| Line::raw(l.clone()))
        .collect::<Vec<_>>();
    let widget = ratatui::widgets::Paragraph::new(text).block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title(format!(" {title} ")),
    );
    frame.render_widget(widget, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::MediaType;
    use image::{Frame, ImageBuffer, Rgba, RgbaImage};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let buf: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let mut out = Vec::new();
        buf.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn gif_two_frames() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new_with_speed(&mut out, 1);
            enc.encode_frame(Frame::new(ImageBuffer::from_pixel(
                16,
                16,
                Rgba([255, 0, 0, 255]),
            )))
            .unwrap();
            enc.encode_frame(Frame::new(ImageBuffer::from_pixel(
                16,
                16,
                Rgba([0, 0, 255, 255]),
            )))
            .unwrap();
        }
        out
    }

    #[test]
    fn decode_png_to_rgba_with_dimensions() {
        let d = decode_image(&png_bytes(640, 480), &MediaType("image/png".into())).unwrap();
        assert_eq!((d.width, d.height), (640, 480));
        assert_eq!(d.rgba.get_pixel(0, 0).0, [0, 0, 128, 255]);
    }

    #[test]
    fn decode_gif_returns_first_frame_only_without_stalling() {
        let t0 = std::time::Instant::now();
        let d = decode_image(&gif_two_frames(), &MediaType("image/gif".into())).unwrap();
        assert!(t0.elapsed().as_millis() < 500, "首帧解码不得卡顿");
        assert_eq!(d.rgba.get_pixel(8, 8).0, [255, 0, 0, 255], "仅首帧（红）");
    }

    #[test]
    fn decode_unsupported_media_type_is_typed_error() {
        let err = decode_image(&[0u8; 4], &MediaType("image/svg+xml".into())).unwrap_err();
        assert_eq!(err.code, "decode/unsupported");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn decode_corrupt_bytes_is_typed_error() {
        let err = decode_image(b"\x89PNG not really", &MediaType("image/png".into())).unwrap_err();
        assert_eq!(err.code, "decode/corrupt");
    }

    #[test]
    fn kitty_capability_detection_is_pure_and_env_driven() {
        assert!(kitty_supported_with(Some("xterm-kitty"), None));
        assert!(kitty_supported_with(Some("xterm-kitty"), Some("1")));
        assert!(kitty_supported_with(Some("dumb"), Some("1")));
        // KITTY_WINDOW_ID 存在即为 Kitty 能力信号——即使值为空串。
        assert!(kitty_supported_with(Some("xterm-256color"), Some("")));
        assert!(!kitty_supported_with(Some("xterm-256color"), None));
        assert!(!kitty_supported_with(None, None));
    }

    #[test]
    fn kitty_frame_downscales_large_image_and_emits_protocol_bytes() {
        let big: image::DynamicImage =
            ImageBuffer::from_fn(1920, 1080, |_, _| image::Rgb::<u8>([7, 7, 7])).into();
        let frame = kitty_frame(big, (8, 16), Rect::new(0, 0, 80, 20), 3).unwrap();
        // 降采样：rect 必须适配视口（fit 保持宽高比）。
        let rect = frame.rect();
        assert!(rect.width <= 80 && rect.height <= 20, "rect={rect:?}");
        assert!(rect.width > 0 && rect.height > 0);

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(ratatui_image::Image::new(frame.as_ref()), f.area());
            })
            .unwrap();
        let found = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol().starts_with('\u{1b}') && cell.symbol().contains("_G"));
        assert!(found, "必须产出 Kitty graphics protocol 帧字节");
    }

    #[test]
    fn placeholder_box_shows_hint_lines() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_placeholder_box(
                    f,
                    Rect::new(0, 0, 60, 8),
                    "ImageView",
                    &["图片不可用: 断网".to_string(), "[q] 关闭".to_string()],
                )
            })
            .unwrap();
        // 宽字符占两个 cell（后一 cell 为占位空格）：跳过占位。
        let mut skip = 0usize;
        let mut text = String::new();
        for cell in terminal.backend().buffer().content() {
            if skip == 0 && !cell.skip {
                text.push_str(cell.symbol());
            }
            skip = std::cmp::max(skip, ratatui::text::Span::raw(cell.symbol()).width())
                .saturating_sub(1);
        }
        assert!(text.contains("图片不可用: 断网"), "text={text}");
        assert!(text.contains("[q] 关闭"), "text={text}");
    }
}
