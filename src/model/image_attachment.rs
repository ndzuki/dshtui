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
}
