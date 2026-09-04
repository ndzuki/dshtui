//! Visual selection + clipboard yank state and block→text extraction
//! (REQ-003 FR-003-03, D-21; Notes/04 §4.4). Memory only (Notes/06 §9).

use crate::model::Block;

/// Visual selection mode (`v` char / `V` line, Notes/04 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualMode {
    Char,
    Line,
}

/// Selection over window block indices. Char mode selects the single anchor
/// block (its whole plain text); Line mode extends across blocks with `j`/`k`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualSelection {
    pub anchor: usize,
    pub cursor: usize,
    pub mode: VisualMode,
}

impl VisualSelection {
    /// Inclusive block-index range [min, max].
    pub fn range(&self) -> (usize, usize) {
        if self.mode == VisualMode::Char || self.anchor == self.cursor {
            (self.anchor, self.anchor)
        } else {
            (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
        }
    }
}

/// Clipboard backend in use (AC-003-08 fallback chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YankBackend {
    #[default]
    Unavailable,
    System,
    Osc52,
}

impl YankBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            YankBackend::Unavailable => "unavailable",
            YankBackend::System => "system",
            YankBackend::Osc52 => "osc52",
        }
    }
}

/// Yank state (memory only; `last` never written to disk or logs).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct YankState {
    pub visual: Option<VisualSelection>,
    pub last: Option<String>,
    pub backend: YankBackend,
    /// One-shot status toast (`copied` / failure hint).
    pub toast: Option<String>,
}

/// What a context yank copies (Notes/04 §4.4 smart-yank table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YankTarget {
    Code(String),
    Link(String),
    Image(String),
    ToolResult(String),
    Paragraph(String),
}

impl YankTarget {
    pub fn content(&self) -> &str {
        match self {
            YankTarget::Code(s)
            | YankTarget::Link(s)
            | YankTarget::Image(s)
            | YankTarget::ToolResult(s)
            | YankTarget::Paragraph(s) => s,
        }
    }
}

/// Context yank target for the focused block (Notes/04 §4.4): code fence →
/// its code (via the search-index extractor), link → URL, tool result →
/// result text, image → name, otherwise the paragraph text.
pub fn block_yank_target(block: &Block) -> Option<YankTarget> {
    match block {
        Block::ToolCall { args_raw, .. } => {
            args_raw.as_ref().map(|v| YankTarget::Code(v.to_string()))
        }
        Block::ToolResult { content, .. } => Some(YankTarget::ToolResult(content.clone())),
        Block::Image { name, .. } => name
            .clone()
            .map(YankTarget::Image)
            .or_else(|| Some(YankTarget::Image("image".into()))),
        Block::AssistantMessage { chunks, .. } => {
            let text = crate::model::search::chunks_text_public(chunks);
            // 代码块优先（AC-003-03：`y` 复制整块代码，Notes/04 §4.4）。
            if let Some((_, code)) = crate::model::search::extract_code_blocks(&text)
                .into_iter()
                .next()
            {
                return Some(YankTarget::Code(code));
            }
            // A link in the focused block wins over the whole paragraph
            // (AC-003-04 `y` copies the URL).
            if let Some((_, url)) = crate::model::search::extract_links(&text)
                .into_iter()
                .next()
            {
                return Some(YankTarget::Link(url));
            }
            Some(YankTarget::Paragraph(text.trim().to_string()))
        }
        Block::UserMessage { content, .. } => {
            if let Some((_, url)) = crate::model::search::extract_links(content)
                .into_iter()
                .next()
            {
                return Some(YankTarget::Link(url));
            }
            Some(YankTarget::Paragraph(content.trim().to_string()))
        }
        _ => None,
    }
}

/// Plain text of a block (visual selection copy source).
pub fn block_plain_text(block: &Block) -> String {
    match block {
        Block::UserMessage { content, .. } => content.clone(),
        Block::AssistantMessage { chunks, .. } => crate::model::search::chunks_text_public(chunks),
        Block::ToolCall { name, args_raw, .. } => match (name, args_raw) {
            (Some(n), Some(a)) => format!("{n} {a}"),
            (Some(n), None) => n.clone(),
            (None, Some(a)) => a.to_string(),
            _ => String::new(),
        },
        Block::ToolResult { content, .. } => content.clone(),
        Block::Image { name, .. } => name.clone().unwrap_or_default(),
        Block::RequestHeader { summary, .. } | Block::Compaction { summary, .. } => summary.clone(),
        Block::Unknown { raw, .. } => raw.to_string(),
    }
}

/// Visual-mode copy text for a block range (char mode trims the single block;
/// line mode joins whole blocks with newlines).
pub fn selection_text(blocks: &[Block], selection: &VisualSelection) -> Option<String> {
    let (start, end) = selection.range();
    if start >= blocks.len() {
        return None;
    }
    match selection.mode {
        VisualMode::Char => Some(block_plain_text(&blocks[start])),
        VisualMode::Line => {
            let mut parts = Vec::new();
            for block in &blocks[start..=end.min(blocks.len() - 1)] {
                parts.push(block_plain_text(block));
            }
            Some(parts.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SessionSeq;
    use crate::api::types::{ChunkData, ChunkRow};
    use crate::model::PackedChunks;

    fn assistant(seq: u64, md: &str) -> Block {
        Block::AssistantMessage {
            seq: SessionSeq(seq),
            chunks: PackedChunks {
                rows: vec![ChunkRow::TextChunks(ChunkData {
                    texts: vec![md.to_string()],
                    ..Default::default()
                })],
            },
            time: None,
        }
    }

    #[test]
    fn link_in_focused_block_yanks_url_ac003_04() {
        let b = assistant(1, "见 [部署文档](https://example.com/x)。");
        let target = block_yank_target(&b).unwrap();
        assert_eq!(target, YankTarget::Link("https://example.com/x".into()));
    }

    #[test]
    fn tool_result_yanks_result_text() {
        let b = Block::ToolResult {
            seq: SessionSeq(2),
            call_id: None,
            content: "✓ 12ms\noutput".into(),
            is_error: false,
            time: None,
        };
        assert_eq!(
            block_yank_target(&b),
            Some(YankTarget::ToolResult("✓ 12ms\noutput".into()))
        );
    }

    #[test]
    fn tool_call_yanks_args_as_code() {
        let b = Block::ToolCall {
            seq: SessionSeq(3),
            call_id: Some("c1".into()),
            name: Some("bash".into()),
            args_raw: Some(serde_json::json!({"command": "ls"})),
            time: None,
        };
        assert_eq!(
            block_yank_target(&b),
            Some(YankTarget::Code(r#"{"command":"ls"}"#.into()))
        );
    }

    #[test]
    fn image_yanks_name() {
        let b = Block::Image {
            seq: SessionSeq(4),
            attachment_id: Some("a1".into()),
            name: Some("design.png".into()),
            dims: None,
        };
        assert_eq!(
            block_yank_target(&b),
            Some(YankTarget::Image("design.png".into()))
        );
    }

    #[test]
    fn selection_char_mode_single_block_line_mode_joins() {
        let blocks = vec![
            Block::UserMessage {
                seq: SessionSeq(1),
                content: "第一行".into(),
                time: None,
            },
            Block::UserMessage {
                seq: SessionSeq(2),
                content: "第二行".into(),
                time: None,
            },
            Block::UserMessage {
                seq: SessionSeq(3),
                content: "第三行".into(),
                time: None,
            },
        ];
        let char_sel = VisualSelection {
            anchor: 1,
            cursor: 2,
            mode: VisualMode::Char,
        };
        assert_eq!(
            selection_text(&blocks, &char_sel).as_deref(),
            Some("第二行")
        );
        let line_sel = VisualSelection {
            anchor: 0,
            cursor: 2,
            mode: VisualMode::Line,
        };
        assert_eq!(
            selection_text(&blocks, &line_sel).as_deref(),
            Some("第一行\n第二行\n第三行")
        );
    }
}
