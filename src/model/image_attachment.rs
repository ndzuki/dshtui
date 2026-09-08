//! Image attachment send state (REQ-007 AC-007-24; pure model).
//!
//! Wire: prompt content image part is `{type:"image",mediaType,data:base64,
//! name?}` — base64 inline (NO path upload RPC). Send order
//! `[image parts..., text]` (official web). imageLimits projection enforces
//! maxImageBytes / maxImagesPerMessage / mediaTypes.

use crate::api::types::MediaType;
use crate::model::image::is_supported_image;

/// One pending image attachment (read + base64-encoded, ready to send).
#[derive(Debug, Clone, PartialEq)]
pub struct ImageAttachment {
    pub path: String,
    pub media_type: MediaType,
    pub data_base64: String,
    pub bytes: usize,
}

/// Attachment send state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageAttachmentState {
    pub pending: Vec<ImageAttachment>,
    /// In-flight send generation (single-flight: one image message at a time).
    pub inflight: bool,
    pub last_error_code: Option<String>,
}

impl ImageAttachmentState {
    pub fn add(&mut self, att: ImageAttachment) {
        self.pending.push(att);
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub fn total_bytes(&self) -> usize {
        self.pending.iter().map(|a| a.bytes).sum()
    }

    /// Validate pending against the official imageLimits projection
    /// (returns a user-facing message when over/unsupported; Ok otherwise).
    pub fn validate(
        &self,
        max_image_bytes: Option<usize>,
        max_images: Option<usize>,
        allowed_media_types: Option<&[String]>,
    ) -> Result<(), String> {
        if let Some(max) = max_images {
            if self.pending.len() > max {
                return Err(format!("图片数量超过上限（{max}）"));
            }
        }
        if let Some(max) = max_image_bytes {
            if self.total_bytes() > max {
                return Err(format!("图片总大小超过上限（{} 字节）", max));
            }
        }
        for a in &self.pending {
            if !is_supported_image(&a.media_type.0) {
                return Err(format!("不支持的图片类型：{}", a.media_type.0));
            }
            if let Some(allowed) = allowed_media_types {
                if !allowed.iter().any(|t| t == &a.media_type.0) {
                    return Err(format!("服务端不允许的图片类型：{}", a.media_type.0));
                }
            }
        }
        Ok(())
    }

    /// D-51：本地软上限校验（config，默认 数量 ≤10 / 单张 ≤20MiB）——逐张
    /// 字节校验（per-image，非合计），与官方 imageLimits（validate）叠加
    /// 双校验：本地更严/投影缺失时本地兜底生效。超限拒绝并提示可重选。
    pub fn validate_local_soft_limit(
        &self,
        max_count: usize,
        max_per_image_bytes: u64,
    ) -> Result<(), String> {
        if self.pending.len() > max_count {
            return Err(format!("图片数量超过本地上限（{} 张，可配置）", max_count));
        }
        for a in &self.pending {
            if a.bytes as u64 > max_per_image_bytes {
                return Err(format!(
                    "单张图片超过本地上限（{} 字节 > {} 字节），请压缩或换图",
                    a.bytes, max_per_image_bytes
                ));
            }
        }
        Ok(())
    }
}

/// Infer media type from a file extension (whitelist; unknown → Err).
pub fn media_type_from_path(path: &str) -> Result<MediaType, String> {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    let mt = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => {
            return Err(format!(
                "无法识别的图片扩展名：.{ext}（支持 png/jpg/jpeg/webp/gif）"
            ))
        }
    };
    Ok(MediaType(mt.to_string()))
}

/// Does this trimmed text look like a LOCAL path (never a remote URL)?
/// Guard for "不把远程路径当本地读取" (AC-007-24): http(s):// and
/// `dsh-attachment:`/session refs are not local paths.
pub fn looks_like_local_path(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return false;
    }
    if t.contains("://") || t.contains("dsh-session:") || t.starts_with('@') {
        return false; // URL / mention / session ref
    }
    // 绝对/相对/家目录/Windows 盘符形态。
    t.starts_with('/')
        || t.starts_with("./")
        || t.starts_with("../")
        || t.starts_with("~/")
        || t.as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && t.as_bytes().get(1) == Some(&b':')
}

/// Classify composer draft lines that are image-path candidates: a whole-line
/// trimmed token that looks local AND has a supported image extension.
/// Non-candidate lines (prose, URLs, other paths) stay as text — never
/// attempted as local file reads.
pub fn image_path_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && looks_like_local_path(l) && media_type_from_path(l).is_ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(bytes: usize) -> ImageAttachment {
        ImageAttachment {
            path: "/a.png".into(),
            media_type: MediaType("image/png".into()),
            data_base64: "x".into(),
            bytes,
        }
    }

    #[test]
    fn media_type_inference_and_rejection() {
        assert_eq!(media_type_from_path("x.PNG").unwrap().0, "image/png");
        assert_eq!(media_type_from_path("y.jpeg").unwrap().0, "image/jpeg");
        assert!(media_type_from_path("z.gif").is_ok());
        assert!(media_type_from_path("z.bmp").is_err());
        assert!(media_type_from_path("noext").is_err());
    }

    #[test]
    fn validate_enforces_limits_and_whitelist() {
        let mut s = ImageAttachmentState::default();
        s.add(png(10));
        s.add(png(20));
        assert!(s
            .validate(Some(100), Some(3), Some(&["image/png".into()]))
            .is_ok());
        assert!(s.validate(Some(25), None, None).is_err(), "总字节超限");
        assert!(s.validate(None, Some(1), None).is_err(), "数量超限");
        assert!(
            s.validate(None, None, Some(&["image/webp".into()]))
                .is_err(),
            "类型不被允许"
        );
    }

    #[test]
    fn add_clear_and_single_flight_flag() {
        let mut s = ImageAttachmentState::default();
        s.add(png(1));
        assert_eq!(s.pending.len(), 1);
        s.clear_pending();
        assert!(s.pending.is_empty());
        assert!(!s.inflight);
        s.inflight = true;
        assert!(s.inflight);
    }

    #[test]
    fn classify_image_lines_and_local_path_guard_ac007_24() {
        // 绝对路径 png/jpg/webp/gif 识别；URL/mention/含空白行不识别。
        assert_eq!(
            image_path_lines(
                "/home/nd/a.png\n看一下这个图\nhttps://x.com/b.png\n@[s](dsh-session:s1)\n./c.jpeg"
            ),
            vec!["/home/nd/a.png", "./c.jpeg"]
        );
        assert_eq!(
            image_path_lines("/a.txt\n/a.svg\nplain text here"),
            Vec::<&str>::new()
        );
        assert_eq!(image_path_lines(""), Vec::<&str>::new());
        // 路径判定防远程/引用误读。
        assert!(!looks_like_local_path("https://x/y.png"));
        assert!(!looks_like_local_path("@[部署](dsh-session:s1)"));
        assert!(!looks_like_local_path("two words.png"));
        assert!(looks_like_local_path("/abs/x.png"));
        assert!(looks_like_local_path("C:\\x.png"));
    }

    #[test]
    fn validate_local_soft_limit_per_image_and_count_d51() {
        let mut s = ImageAttachmentState::default();
        // 数量：3 张 > 2 上限 → 拒。
        s.add(png(100));
        s.add(png(100));
        s.add(png(100));
        assert!(
            s.validate_local_soft_limit(2, 1000).is_err(),
            "数量超本地软上限"
        );
        // 单张：25 字节 > 20 上限（逐张，非合计）→ 拒；另两张小图不连坐。
        let mut s2 = ImageAttachmentState::default();
        s2.add(png(10));
        s2.add(png(25));
        s2.add(png(5));
        assert!(
            s2.validate_local_soft_limit(10, 20).is_err(),
            "单张超 20 字节本地上限"
        );
        let mut s3 = ImageAttachmentState::default();
        s3.add(png(10));
        s3.add(png(19));
        assert!(
            s3.validate_local_soft_limit(2, 20).is_ok(),
            "合计 29 但逐张均 ≤20 → 通过（per-image 语义）"
        );
        // 边界：恰好 = 上限通过。
        let mut s4 = ImageAttachmentState::default();
        s4.add(png(20));
        assert!(s4.validate_local_soft_limit(10, 20).is_ok());
    }
}
