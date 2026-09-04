//! Structured window search index (REQ-003 FR-003-02/03, D-21; Notes/06 §4).
//!
//! - The index mirrors the transcript window: rebuilt on every window change,
//!   never persisted (Notes/06 §9).
//! - `nucleo` Matcher scores fuzzy matches (ADR-003); the hit order stays in
//!   window order so `n`/`N` navigation is deterministic.
//! - Markdown text extraction (code blocks / links) lives here so the model
//!   layer stays pure (`pulldown-cmark` has no ratatui dependency); the UI
//!   markdown renderer re-parses independently for display.

use nucleo::Matcher;

use crate::api::types::SessionSeq;
use crate::model::{Block, PackedChunks};

/// What a searchable item is (prefix filters `/c` `/l` `/i` `/t`, Notes/04 §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchKind {
    Text,
    Code { lang: Option<String> },
    Link { url: String },
    Image { name: String },
    ToolCall { name: String },
}

impl SearchKind {
    /// Overlay label for the match list (Notes/05 §4).
    pub fn label(&self) -> &'static str {
        match self {
            SearchKind::Text => "text",
            SearchKind::Code { .. } => "code",
            SearchKind::Link { .. } => "link",
            SearchKind::Image { .. } => "image",
            SearchKind::ToolCall { .. } => "tool",
        }
    }
}

/// Prefix filter for a search query (`/c` `/l` `/i` `/t`; bare query = all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchKindFilter {
    #[default]
    All,
    Code,
    Link,
    Image,
    ToolCall,
}

impl SearchKindFilter {
    pub fn from_prefix(query: &str) -> (Self, &str) {
        if let Some(rest) = query.strip_prefix("/c ") {
            (Self::Code, rest)
        } else if let Some(rest) = query.strip_prefix("/l ") {
            (Self::Link, rest)
        } else if let Some(rest) = query.strip_prefix("/i ") {
            (Self::Image, rest)
        } else if let Some(rest) = query.strip_prefix("/t ") {
            (Self::ToolCall, rest)
        } else {
            (Self::All, query)
        }
    }

    pub fn matches(&self, kind: &SearchKind) -> bool {
        match self {
            SearchKindFilter::All => true,
            SearchKindFilter::Code => matches!(kind, SearchKind::Code { .. }),
            SearchKindFilter::Link => matches!(kind, SearchKind::Link { .. }),
            SearchKindFilter::Image => matches!(kind, SearchKind::Image { .. }),
            SearchKindFilter::ToolCall => matches!(kind, SearchKind::ToolCall { .. }),
        }
    }
}

/// One indexed item (Notes/06 §4 `SearchIndex.item`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchItem {
    pub kind: SearchKind,
    pub seq: SessionSeq,
    /// Searchable text (code content / URL / args / result).
    pub text: String,
    /// List row label (kind + short digest).
    pub display: String,
}

/// One query hit: item index into `items`, fuzzy score, highlight positions.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchMatch {
    pub item_index: usize,
    pub score: u16,
    /// Char positions of the match inside `text` (highlight range).
    pub positions: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct SearchIndex {
    items: Vec<SearchItem>,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[SearchItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Rebuild the index from the current window blocks (window order).
    pub fn rebuild(&mut self, blocks: &[Block]) {
        self.items.clear();
        for block in blocks {
            push_block_items(&mut self.items, block);
        }
    }

    /// Fuzzy query over the index (ADR-003 nucleo). Empty query → no matches;
    /// matches stay in window order (n/N navigation contract).
    pub fn query(&self, query: &str, filter: SearchKindFilter) -> Vec<SearchMatch> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let mut matcher = Matcher::new(nucleo::Config::DEFAULT);
        let mut needle_buf = Vec::new();
        let needle = nucleo::Utf32Str::new(query, &mut needle_buf);
        let mut out = Vec::new();
        for (idx, item) in self.items.iter().enumerate() {
            if !filter.matches(&item.kind) {
                continue;
            }
            let mut positions = Vec::new();
            let mut hay_buf = Vec::new();
            let haystack = nucleo::Utf32Str::new(&item.text, &mut hay_buf);
            if let Some(score) = matcher.fuzzy_indices(haystack, needle, &mut positions) {
                out.push(SearchMatch {
                    item_index: idx,
                    score,
                    positions,
                });
            }
        }
        out
    }
}

fn push_block_items(out: &mut Vec<SearchItem>, block: &Block) {
    let seq = block.seq();
    match block {
        Block::UserMessage { content, .. } => {
            push_text_item(out, seq, content);
        }
        Block::AssistantMessage { chunks, .. } => {
            let text = chunks_text(chunks);
            push_markdown_items(out, seq, &text);
        }
        Block::ToolCall { name, args_raw, .. } => {
            let name = name.clone().unwrap_or_else(|| "tool".into());
            let args = args_raw.as_ref().map(|v| v.to_string()).unwrap_or_default();
            out.push(SearchItem {
                kind: SearchKind::ToolCall { name: name.clone() },
                seq,
                text: format!("{name} {args}"),
                display: format!("tool: {name}"),
            });
        }
        Block::ToolResult { content, .. } => {
            push_text_item(out, seq, content);
        }
        Block::Image { name, .. } => {
            let name = name.clone().unwrap_or_else(|| "image".into());
            out.push(SearchItem {
                kind: SearchKind::Image { name: name.clone() },
                seq,
                text: name.clone(),
                display: format!("image: {name}"),
            });
        }
        // RequestHeader / Compaction / Unknown: not user content; they stay
        // out of the index (Notes/06 §4 kinds list).
        _ => {}
    }
}

fn push_text_item(out: &mut Vec<SearchItem>, seq: SessionSeq, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let digest: String = text.chars().take(60).collect();
    out.push(SearchItem {
        kind: SearchKind::Text,
        seq,
        text: text.to_string(),
        display: format!("text: {digest}"),
    });
}

/// Markdown-aware indexing of assistant text: fenced code blocks become Code
/// items, links Link items, remaining paragraphs a Text item (each with its
/// own kind so `/c` `/l` filters and context yank can address them).
fn push_markdown_items(out: &mut Vec<SearchItem>, seq: SessionSeq, text: &str) {
    for (lang, code) in extract_code_blocks(text) {
        let lang_digest = lang.as_deref().unwrap_or("code").to_string();
        out.push(SearchItem {
            kind: SearchKind::Code { lang },
            seq,
            text: code.clone(),
            display: format!("code: {lang_digest}"),
        });
    }
    for (label, url) in extract_links(text) {
        out.push(SearchItem {
            kind: SearchKind::Link { url: url.clone() },
            seq,
            text: format!("{label} {url}"),
            display: format!("link: {label}"),
        });
    }
    // Paragraph text outside code fences: searchable as Text.
    let paragraphs = strip_code_fences(text);
    if !paragraphs.trim().is_empty() {
        push_text_item(out, seq, &paragraphs);
    }
}

/// Concatenated text of the packed chunk rows (same seam the UI renders; also
/// the copy source for visual selection, model/yank.rs).
/// Concatenated text of the packed chunk rows（行间以 \n 连接，保留换行
/// 语义；搜索索引、上下文复制与 markdown 渲染共用同一点——ui/chat.rs 不再
/// 单独拼接，避免两处语义分化）。
pub fn chunks_text(chunks: &PackedChunks) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(chunks.rows.len());
    for row in &chunks.rows {
        let part = match row {
            crate::api::types::ChunkRow::TextChunks(d)
            | crate::api::types::ChunkRow::ReasoningChunks(d) => d.texts.join(""),
            crate::api::types::ChunkRow::ToolCallChunks(t) => {
                let mut s = String::new();
                if let Some(n) = &t.name {
                    s.push_str(n);
                }
                if let Some(a) = &t.args {
                    s.push(' ');
                    s.push_str(&a.to_string());
                }
                s
            }
            crate::api::types::ChunkRow::Unknown { .. } => continue,
        };
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join("\n")
}

/// Extract fenced code blocks `(lang, code)` in document order (pulldown-cmark).
pub fn extract_code_blocks(text: &str) -> Vec<(Option<String>, String)> {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

    let parser = Parser::new(text);
    let mut out = Vec::new();
    let mut current_lang: Option<Option<String>> = None;
    let mut buf = String::new();
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
                current_lang = Some(if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_string())
                });
                buf.clear();
            }
            Event::Text(t) => {
                if current_lang.is_some() {
                    buf.push_str(&t);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(lang) = current_lang.take() {
                    out.push((lang, std::mem::take(&mut buf)));
                }
            }
            _ => {}
        }
    }
    out
}

/// Extract markdown links `(label, url)` in document order.
pub fn extract_links(text: &str) -> Vec<(String, String)> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let parser = Parser::new(text);
    let mut out = Vec::new();
    let mut active: Option<String> = None;
    let mut label = String::new();
    for event in parser {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                active = Some(dest_url.to_string());
                label.clear();
            }
            Event::Text(t) => {
                if active.is_some() {
                    label.push_str(&t);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = active.take() {
                    out.push((std::mem::take(&mut label), url));
                }
            }
            _ => {}
        }
    }
    out
}

/// Markdown text with fenced code blocks removed (paragraph-only text).
fn strip_code_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{ChunkData, ChunkRow};

    fn assistant_md(seq: u64, md: &str) -> Block {
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
    fn index_extracts_code_links_and_text_from_markdown() {
        let blocks = vec![assistant_md(
            3,
            "# 标题\n\n```rust\nfn main() {}\n```\n\n见 [文档](https://example.com) 部署。",
        )];
        let mut index = SearchIndex::new();
        index.rebuild(&blocks);
        let kinds: Vec<_> = index.items().iter().map(|i| &i.kind).collect();
        assert!(kinds
            .iter()
            .any(|k| matches!(k, SearchKind::Code { lang } if lang.as_deref() == Some("rust"))));
        assert!(kinds
            .iter()
            .any(|k| matches!(k, SearchKind::Link { url } if url == "https://example.com")));
        assert!(kinds.iter().any(|k| matches!(k, SearchKind::Text)));
    }

    #[test]
    fn code_block_item_carries_full_code_for_yank_ac003_03() {
        let blocks = vec![assistant_md(4, "```json\n{\"key\": \"json-value\"}\n```")];
        let mut index = SearchIndex::new();
        index.rebuild(&blocks);
        let m = index.query("json", SearchKindFilter::Code);
        assert_eq!(m.len(), 1);
        let item = &index.items()[m[0].item_index];
        assert!(matches!(item.kind, SearchKind::Code { .. }));
        assert_eq!(item.text, "{\"key\": \"json-value\"}\n");
    }

    #[test]
    fn prefix_filters_route_by_kind() {
        assert_eq!(
            SearchKindFilter::from_prefix("/c deploy").0,
            SearchKindFilter::Code
        );
        assert_eq!(
            SearchKindFilter::from_prefix("/l http").0,
            SearchKindFilter::Link
        );
        assert_eq!(
            SearchKindFilter::from_prefix("/i png").0,
            SearchKindFilter::Image
        );
        assert_eq!(
            SearchKindFilter::from_prefix("/t bash").0,
            SearchKindFilter::ToolCall
        );
        assert_eq!(
            SearchKindFilter::from_prefix("deploy").0,
            SearchKindFilter::All
        );
        assert_eq!(SearchKindFilter::from_prefix("/c deploy").1, "deploy");
    }

    #[test]
    fn empty_query_yields_no_matches_ac003_19() {
        let blocks = vec![assistant_md(1, "hello world")];
        let mut index = SearchIndex::new();
        index.rebuild(&blocks);
        assert!(index.query("", SearchKindFilter::All).is_empty());
        assert!(index.query("   ", SearchKindFilter::All).is_empty());
    }

    #[test]
    fn fuzzy_match_returns_highlight_positions() {
        let blocks = vec![assistant_md(2, "deploy the operator")];
        let mut index = SearchIndex::new();
        index.rebuild(&blocks);
        let m = index.query("deplo", SearchKindFilter::All);
        assert_eq!(m.len(), 1);
        assert!(!m[0].positions.is_empty());
    }

    #[test]
    fn extract_helpers_are_order_stable() {
        let md = "```rust\na\n```\n```sh\nb\n```\n[x](u1) [y](u2)";
        let code = extract_code_blocks(md);
        assert_eq!(code.len(), 2);
        assert_eq!(code[0].0.as_deref(), Some("rust"));
        assert_eq!(code[0].1, "a\n");
        assert_eq!(code[1].1, "b\n");
        let links = extract_links(md);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0], ("x".into(), "u1".into()));
        assert_eq!(links[1], ("y".into(), "u2".into()));
    }
}
