//! Markdown 渲染（REQ-003 FR-003-01/AC-003-01/02；Notes/04 §4、Notes/06 §5）。
//!
//! - 标题/列表/表格/行内代码由 `ratatui-markdown`（pulldown-cmark 内核）渲染；
//! - 闭合代码块经 `RenderHooks::render_code_block` 交 `syntect` 语法高亮
//!   （懒执行：SyntaxSet/Theme 进程级 OnceLock 缓存）；
//! - 流式中未闭合代码块不进 markdown 路径（先纯文本，AC-003-02），判定用
//!   `has_unclosed_fence`（pulldown-cmark 事件流，不用行计数——代码块内以
//!   ``` 开头的示例行不会被误判）；
//! - `markdown_lines` 结果按（内容哈希, 宽度）进程级 LRU 缓存（Notes/06 §5
//!   懒执行 + 缓存当前窗口；流式尾块每次内容变化哈希不同，自动失效）。
//!
//! 纯函数层：不依赖 AppState/transport，TestBackend golden 与单测直接断言。

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui_markdown::markdown::{MarkdownRenderer, RenderHooks};
use ratatui_markdown::theme::ThemeConfig;

/// 进程级缓存：默认语法集（换行版）与高亮主题（懒加载一次）。
static SYNTAX_SET: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
static HIGHLIGHT_THEME: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
/// markdown 渲染缓存行类型。
type RenderLines = Arc<Vec<Line<'static>>>;
/// markdown 渲染结果缓存（(内容哈希, 宽度) → 行）；上限 128 条，超出整表
/// 清空（窗口有界，重建成本可接受，防止流式内容无限膨胀）。
static RENDER_CACHE: OnceLock<Mutex<HashMap<(u64, usize), RenderLines>>> = OnceLock::new();
const RENDER_CACHE_CAP: usize = 128;

/// markdown → styled lines（纯函数 + 缓存）。宽 0 时按最小 20 列渲染。
pub fn markdown_lines(md: &str, width: usize) -> RenderLines {
    let width = width.max(20);
    let mut hasher = DefaultHasher::new();
    md.hash(&mut hasher);
    let key = (hasher.finish(), width);
    let cache = RENDER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().expect("cache lock").get(&key) {
        return Arc::clone(hit);
    }
    let renderer = MarkdownRenderer::new(width).with_render_hooks(Box::new(HighlightHooks));
    let blocks = renderer.parse(md);
    let lines = Arc::new(renderer.render(&blocks, &ThemeConfig::default()));
    let mut guard = cache.lock().expect("cache lock");
    if guard.len() >= RENDER_CACHE_CAP {
        guard.clear();
    }
    guard.insert(key, Arc::clone(&lines));
    lines
}

/// 代码块是否未闭合（CommonMark 行级状态机：pulldown-cmark 在 EOF 处隐式
/// 闭合围栏，与 AC-003-02「流式中未闭合块先纯文本」口径不符，故手工判定）：
/// - 围栏打开后，代码内容行以 ``` 开头不误判（只认「反引号串后仅空白且
///   长度 ≥ 打开串」的闭合围栏）；
/// - 到文本末尾仍打开 → 未闭合（流式先纯文本）。
pub fn has_unclosed_fence(text: &str) -> bool {
    let mut open_len: Option<usize> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("```") else {
            continue;
        };
        let run = 3 + rest.len() - rest.trim_start_matches('`').len();
        match open_len {
            Some(opener) => {
                // 闭合围栏：反引号串后仅空白，且长度 ≥ 打开串。
                if run >= opener && rest.trim_start_matches('`').trim().is_empty() {
                    open_len = None;
                }
                // 否则为代码内容行（如 ```example），忽略。
            }
            None => open_len = Some(run),
        }
    }
    open_len.is_some()
}

struct HighlightHooks;

impl RenderHooks for HighlightHooks {
    fn render_code_block(&self, lang: &str, content: &str) -> Option<Vec<Line<'static>>> {
        Some(highlight_code(lang, content))
    }
}

/// 语法高亮一个闭合代码块（AC-003-02：rust/shell/json/python 等）。未知语言
/// 回退 plain text（仍保留行结构，不丢内容）。
pub fn highlight_code(lang: &str, code: &str) -> Vec<Line<'static>> {
    let syntax_set = SYNTAX_SET.get_or_init(syntect::parsing::SyntaxSet::load_defaults_newlines);
    let theme = HIGHLIGHT_THEME.get_or_init(|| {
        syntect::highlighting::ThemeSet::load_defaults().themes["base16-ocean.dark"].clone()
    });
    let syntax = syntax_set
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = syntect::easy::HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in code.lines() {
        match highlighter.highlight_line(line, syntax_set) {
            Ok(ranges) => {
                let spans = ranges
                    .into_iter()
                    .map(|(style, text)| {
                        let mut span_style =
                            Style::default().fg(syntect_to_ratatui(style.foreground));
                        if style
                            .font_style
                            .contains(syntect::highlighting::FontStyle::BOLD)
                        {
                            span_style = span_style.add_modifier(Modifier::BOLD);
                        }
                        if style
                            .font_style
                            .contains(syntect::highlighting::FontStyle::ITALIC)
                        {
                            span_style = span_style.add_modifier(Modifier::ITALIC);
                        }
                        Span::styled(text.to_string(), span_style)
                    })
                    .collect::<Vec<_>>();
                out.push(Line::from(spans));
            }
            Err(_) => out.push(Line::raw(line.to_string())),
        }
    }
    out
}

fn syntect_to_ratatui(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_headings_list_table_inline_code_ac003_01() {
        let md = "# 标题一\n\n- 列表项甲\n- 列表项乙\n\n| 列A | 列B |\n|-----|-----|\n| a | b |\n\n行内 `code` 与普通文字。";
        let lines = markdown_lines(md, 60);
        let text = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("标题一"), "text={text}");
        assert!(
            text.contains("列表项甲") && text.contains("列表项乙"),
            "text={text}"
        );
        assert!(text.contains("列A") && text.contains("列B"), "text={text}");
        assert!(text.contains("code"), "行内代码保留, text={text}");
        assert!(text.contains("普通文字"), "text={text}");
    }

    #[test]
    fn code_block_highlights_known_languages_ac003_02() {
        // rust/shell/json/python 四样例：行结构完整且高亮产生非默认颜色。
        let samples = [
            ("rust", "fn main() {\n    println!(\"hi\");\n}\n"),
            ("shell", "#!/bin/sh\necho hi\n"),
            ("json", "{\"key\": \"value\"}\n"),
            ("python", "def f():\n    return 1\n"),
        ];
        for (lang, code) in samples {
            let lines = highlight_code(lang, code);
            assert!(!lines.is_empty(), "{lang} 高亮行非空");
            let joined = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<String>();
            assert!(
                joined.contains("fn main")
                    || joined.contains("echo")
                    || joined.contains("key")
                    || joined.contains("def f"),
                "{lang}: {joined}"
            );
            let styled = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|s| s.style.fg != Some(Color::Reset) && s.style.fg.is_some());
            assert!(styled, "{lang} 应有非默认前景色（语法高亮生效）");
        }
    }

    #[test]
    fn unknown_language_falls_back_to_plain_text_without_loss() {
        let lines = highlight_code("not-a-lang", "line1\nline2\n");
        let joined = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(joined, "line1|line2");
    }

    #[test]
    fn unclosed_fence_detection_ac003_02() {
        assert!(has_unclosed_fence("```rust\nfn main() {}"));
        assert!(!has_unclosed_fence("```rust\nfn main() {}\n```"));
        assert!(!has_unclosed_fence("纯文本没有围栏"));
        // 行内出现的 ``` 不算围栏。
        assert!(!has_unclosed_fence("说明 `code` 的用法\n"));
        // 代码块内以 ``` 开头的示例行不误判（只认等长闭合围栏）。
        assert!(!has_unclosed_fence("```\n```example\nlet x = 1;\n```"));
        // 更长的闭合围栏（≥ 打开串）也可关闭。
        assert!(!has_unclosed_fence("```rust\nfn main() {}\n````"));
    }
}
